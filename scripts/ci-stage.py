#!/usr/bin/env python3
"""Capture fixed CI stages without retaining their full output or changing tests."""

from __future__ import annotations

import argparse
import ctypes
import json
import math
import os
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
STAGES = (
    "toolchain-apple", "install-shellcheck", "toolchain-rust", "helper-regressions",
    "pre-commit", "tvos-build", "macos-build", "rust-build", "tvos-test-build",
    "tvos-tests", "core-tests", "macos-tests", "lan-probe", "rust-tests",
)
MAX_LOG_BYTES = 512 * 1024
HEAD_BYTES = 64 * 1024
MAX_JSON_BYTES = 4096
MAX_SUMMARY_BYTES = 64 * 1024
MAX_ARTIFACT_BYTES = 8 * 1024 * 1024
WORK_SECONDS = 45 * 60
HANDLED_SIGNALS = tuple(getattr(signal, name) for name in ("SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT")
                        if hasattr(signal, name))
RESULT_KEYS = {
    "schema_version", "stage", "status", "duration_seconds", "actual_exit_code",
    "exit_code", "timed_out", "interrupted_signal", "log_truncated", "observed_bytes",
    "retained_bytes", "cleanup_state", "error_type", "test_summary",
}


class StageError(RuntimeError):
    pass


class HeadTail:
    def __init__(self) -> None:
        self.head = bytearray()
        self.tail = bytearray()
        self.observed = 0

    def feed(self, chunk: bytes) -> None:
        self.observed += len(chunk)
        take = min(len(chunk), HEAD_BYTES - len(self.head))
        self.head.extend(chunk[:take])
        self.tail.extend(chunk[take:])
        overflow = len(self.tail) - (MAX_LOG_BYTES - HEAD_BYTES)
        if overflow > 0:
            del self.tail[:overflow]


def work_deadline(configured: str | None, timeout_seconds: float) -> float:
    if not math.isfinite(timeout_seconds) or timeout_seconds <= 0:
        raise StageError("Invalid work budget.")
    now = time.monotonic()
    deadline = now + min(timeout_seconds, WORK_SECONDS)
    if configured is not None:
        try:
            absolute = float(configured)
        except (ValueError, TypeError, OverflowError):
            raise StageError("Invalid work budget.") from None
        if not math.isfinite(absolute):
            raise StageError("Invalid work budget.")
        deadline = min(deadline, absolute)
    if deadline <= now:
        raise StageError("Work budget expired.")
    return deadline


def _exited(process: subprocess.Popen[bytes]) -> bool:
    if process.returncode is not None:
        raise StageError("Owned leader was reaped before group cleanup.")
    status = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    return status is not None and status.si_pid == process.pid


def _group_alive(pgid: int) -> bool:
    if sys.platform == "darwin":
        library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        query = library.proc_listpgrppids
        query.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
        query.restype = ctypes.c_int
        pids = (ctypes.c_int * 4096)()
        ctypes.set_errno(0)
        result = query(pgid, pids, ctypes.sizeof(pids))
        if result < 0 or (result == 0 and ctypes.get_errno()) or all(pids):
            raise StageError("Owned group query is unverified.")
        for pid in pids:
            if pid:
                try:
                    if os.getpgid(pid) == pgid:
                        return True
                except ProcessLookupError:
                    pass
        return False
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return False
    if sys.platform.startswith("linux"):
        observed = False
        deadline = time.monotonic() + 1
        with os.scandir("/proc") as entries:
            for count, entry in enumerate(entries):
                if count >= 4096 or time.monotonic() >= deadline:
                    raise StageError("Owned group query exceeded its bound.")
                if not entry.name.isdigit():
                    continue
                try:
                    with (Path(entry.path) / "stat").open("rb") as reader:
                        raw = reader.read(4097)
                except (FileNotFoundError, ProcessLookupError):
                    continue
                if len(raw) > 4096:
                    raise StageError("Owned process stat exceeded its bound.")
                fields = raw.rpartition(b") ")[2].split()
                if len(fields) < 3:
                    raise StageError("Owned process stat is unverified.")
                if int(fields[2]) == pgid:
                    observed = True
                    if fields[0] not in (b"Z", b"X", b"x"):
                        return True
        if observed:
            return False
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return False
        raise StageError("Owned group is not visible.")
    return True


def _stop_group(process: subprocess.Popen[bytes]) -> None:
    # Reserve the PGID with the unreaped leader until every signal is sent.
    if not _exited(process):
        if os.getpgid(process.pid) != process.pid or os.getsid(process.pid) != process.pid:
            raise StageError("Owned process group identity changed.")
    for signum in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, signum)
        except ProcessLookupError:
            pass
        except PermissionError:
            if _group_alive(process.pid):
                raise
        if signum == signal.SIGTERM:
            deadline = time.monotonic() + 1
            while not _exited(process) and time.monotonic() < deadline:
                time.sleep(0.02)
    process.wait(timeout=5)
    deadline = time.monotonic() + 2
    while _group_alive(process.pid):
        if time.monotonic() >= deadline:
            raise StageError("Owned process group cleanup is unverified.")
        time.sleep(0.02)


def _write_json(path: Path, payload: object, limit: int) -> None:
    data = (json.dumps(payload, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")
    if len(data) > limit:
        raise StageError("JSON exceeded its bound.")
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("xb") as writer:
        writer.write(data)
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def run_stage(
    stage: str, command: list[str], artifact_dir: Path, *,
    timeout_seconds: float = WORK_SECONDS, cwd: Path = ROOT,
) -> tuple[int, dict[str, object]]:
    if stage not in STAGES or not command:
        raise StageError("Unknown stage or missing command.")
    artifact_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    log_path = artifact_dir / f"{stage}.log"
    json_path = artifact_dir / f"{stage}.json"
    if json_path.exists():
        raise StageError("Stage already has a result.")
    started = time.monotonic()
    output = HeadTail()
    process: subprocess.Popen[bytes] | None = None
    selector = selectors.DefaultSelector()
    timed_out = False
    interrupted: list[int] = []
    cleanup_state = "not_started"
    error_type: str | None = None
    previous_handlers = {}
    actual_exit: int | None = None

    def on_signal(signum: int, _frame: object) -> None:
        if not interrupted:
            interrupted.append(signum)

    def drain(events: list[tuple[selectors.SelectorKey, int]]) -> None:
        for key, _ in events:
            chunk = os.read(key.fd, 64 * 1024)
            if chunk:
                previous_head = len(output.head)
                output.feed(chunk)
                log_file.write(output.head[previous_head:])
                log_file.flush()
            else:
                selector.unregister(key.fileobj)

    with log_path.open("xb") as log_file:
        try:
            for signum in HANDLED_SIGNALS:
                previous_handlers[signum] = signal.signal(signum, on_signal)
            deadline = work_deadline(os.environ.get("CI_WORK_DEADLINE_MONOTONIC"), timeout_seconds)
            process = subprocess.Popen(
                command, cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT, start_new_session=True, close_fds=True,
            )
            cleanup_state = "unverified"
            assert process.stdout is not None
            os.set_blocking(process.stdout.fileno(), False)
            selector.register(process.stdout, selectors.EVENT_READ)
            while not interrupted:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    timed_out = True
                    break
                drain(selector.select(min(0.1, remaining)))
                if _exited(process):
                    break
        except Exception as error:
            error_type = type(error).__name__
        finally:
            if process is not None:
                try:
                    _stop_group(process)
                    cleanup_state = "verified"
                except Exception as error:
                    error_type = type(error).__name__
                actual_exit = process.returncode
                try:
                    # A descendant may inherit stdout, even after the leader exits.
                    drain_deadline = time.monotonic() + 2
                    while selector.get_map() and time.monotonic() < drain_deadline:
                        drain(selector.select(0.05))
                    if selector.get_map():
                        cleanup_state = "unverified"
                except Exception as error:
                    cleanup_state = "unverified"
                    error_type = type(error).__name__
                if process.stdout is not None:
                    process.stdout.close()
            selector.close()
            log_file.write(output.tail)
            for signum, handler in previous_handlers.items():
                signal.signal(signum, handler)

    if interrupted:
        exit_code = 128 + interrupted[0]
    elif error_type is not None or cleanup_state != "verified":
        exit_code = 125
    elif timed_out:
        exit_code = 124
    elif actual_exit is not None:
        exit_code = actual_exit if actual_exit >= 0 else 128 - actual_exit
    else:
        exit_code = 125
    result: dict[str, object] = {
        "schema_version": 1, "stage": stage,
        "status": "passed" if exit_code == 0 else "failed",
        "duration_seconds": round(time.monotonic() - started, 3),
        "actual_exit_code": actual_exit, "exit_code": exit_code,
        "timed_out": timed_out, "interrupted_signal": interrupted[0] if interrupted else None,
        "log_truncated": output.observed > MAX_LOG_BYTES,
        "observed_bytes": output.observed,
        "retained_bytes": len(output.head) + len(output.tail),
        "cleanup_state": cleanup_state, "error_type": error_type,
        "test_summary": None,
    }
    _write_json(json_path, result, MAX_JSON_BYTES)
    print(f"[ci-stage] {stage}: {result['status']}; exit={exit_code}; "
          f"duration={result['duration_seconds']}s; log={stage}.log", flush=True)
    return exit_code, result


def summarize(artifact_dir: Path) -> dict[str, object]:
    artifact_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    stages = []
    total_bytes = 0
    for stage in STAGES:
        for suffix, limit in (("log", MAX_LOG_BYTES), ("json", MAX_JSON_BYTES)):
            path = artifact_dir / f"{stage}.{suffix}"
            if path.is_symlink():
                raise StageError("Artifact is not a regular file.")
            if path.exists():
                if not path.is_file() or path.stat().st_size > limit:
                    raise StageError("Artifact exceeded its bound.")
                total_bytes += path.stat().st_size
        path = artifact_dir / f"{stage}.json"
        if not path.exists():
            stages.append({"stage": stage, "status": "unavailable", "test_summary": None})
            continue
        with path.open("rb") as reader:
            payload = json.loads(reader.read(MAX_JSON_BYTES + 1))
        if (not isinstance(payload, dict) or set(payload) != RESULT_KEYS
                or type(payload.get("schema_version")) is not int or payload["schema_version"] != 1
                or payload.get("stage") != stage
                or payload.get("status") not in ("passed", "failed")
                or payload.get("test_summary") is not None
                or payload.get("cleanup_state") not in ("verified", "unverified", "not_started")):
            raise StageError("Invalid stage JSON.")
        duration = payload["duration_seconds"]
        if type(duration) not in (int, float) or not math.isfinite(duration) or duration < 0:
            raise StageError("Invalid stage duration.")
        for key in ("exit_code", "observed_bytes", "retained_bytes"):
            if type(payload[key]) is not int or payload[key] < 0:
                raise StageError("Invalid stage counter.")
        for key in ("actual_exit_code", "interrupted_signal"):
            if payload[key] is not None and type(payload[key]) is not int:
                raise StageError("Invalid stage exit.")
        if (type(payload["timed_out"]) is not bool or type(payload["log_truncated"]) is not bool
                or payload["retained_bytes"] > MAX_LOG_BYTES
                or payload["retained_bytes"] > payload["observed_bytes"]
                or payload["log_truncated"] != (payload["observed_bytes"] > MAX_LOG_BYTES)
                or (payload["error_type"] is not None and (
                    not isinstance(payload["error_type"], str)
                    or not payload["error_type"].isidentifier() or len(payload["error_type"]) > 64))):
            raise StageError("Invalid stage output state.")
        passed = (payload["exit_code"] == 0 and payload["actual_exit_code"] == 0
                  and payload["cleanup_state"] == "verified" and not payload["timed_out"]
                  and payload["interrupted_signal"] is None and payload["error_type"] is None)
        if (payload["status"] == "passed") != passed:
            raise StageError("Invalid stage success state.")
        stages.append(payload)
    caches = {}
    for language in ("rust", "swift"):
        prefix = f"CI_{language.upper()}_CACHE_"
        outcome = os.environ.get(prefix + "OUTCOME")
        hit = os.environ.get(prefix + "HIT")
        caches[language] = "hit" if outcome == "success" and hit == "true" else (
            "miss" if outcome == "success" and hit in ("", "false") else "unavailable"
        )
    result = {"schema_version": 1, "stages": stages, "cache_restores": caches}
    encoded_size = len((json.dumps(result, sort_keys=True) + "\n").encode("utf-8"))
    if total_bytes + encoded_size > MAX_ARTIFACT_BYTES:
        raise StageError("Artifact set exceeded its bound.")
    _write_json(artifact_dir / "summary.json", result, MAX_SUMMARY_BYTES)
    lines = ["## CI stages", "", "| Stage | Result | Seconds |", "| --- | --- | ---: |"]
    for stage in stages:
        lines.append(f"| {stage['stage']} | {stage['status']} | {stage.get('duration_seconds', '-')} |")
    lines.extend(["", f"Cache restore: Rust **{caches['rust']}**, Swift **{caches['swift']}**.",
                  "Test counts are unavailable; stage success is based on command exit and cleanup, not inferred counts."])
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a", encoding="utf-8") as writer:
            writer.write("\n".join(lines) + "\n")
    print("[ci-stage] wrote bounded summary.json; cache restore is not performance proof.")
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", choices=STAGES)
    parser.add_argument("--summarize", action="store_true")
    parser.add_argument("--artifact-dir", type=Path,
                        default=os.environ.get("CI_STAGE_ARTIFACT_DIR"))
    parser.add_argument("--timeout-seconds", type=float, default=WORK_SECONDS)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if args.artifact_dir is None or (args.summarize == bool(args.stage)):
        parser.error("Choose a stage or summary and provide an artifact directory.")
    try:
        if args.summarize:
            summarize(args.artifact_dir)
            return 0
        command = args.command[1:] if args.command[:1] == ["--"] else args.command
        return run_stage(args.stage, command, args.artifact_dir,
                         timeout_seconds=args.timeout_seconds)[0]
    except Exception as error:
        print(f"[ci-stage] failed: {type(error).__name__}", file=sys.stderr)
        return 125


if __name__ == "__main__":
    raise SystemExit(main())
