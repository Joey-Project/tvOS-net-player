#!/usr/bin/env python3
"""Run credential-free UI playback tests against an isolated local cache server."""

from __future__ import annotations

import argparse
import base64
import ctypes
import json
import os
import re
import selectors
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable


ROOT = Path(__file__).resolve().parent.parent
SERVER_NAME = "tvos-net-player-cache-server"
FIXTURE_COUNT = 55
FIXTURE_DURATION_SECONDS = 92
MAX_FIXTURE_BYTES = 64 * 1024 * 1024
MAX_RESULT_SUMMARY_BYTES = 2 * 1024 * 1024
MAX_COMMAND_LOG_BYTES = 8 * 1024 * 1024
MAX_SERVER_LOG_BYTES = 256 * 1024
MAX_TASK_FIXTURE_LOG_BYTES = 256 * 1024
MAX_FIXTURE_JSON_BYTES = 64 * 1024
MAX_ARTIFACT_BYTES = 32 * 1024 * 1024
MAX_RESULT_BUNDLE_BYTES = 20 * 1024 * 1024
MAX_ARTIFACT_REPORT_BYTES = 8192
FAILURE_ATTACHMENT_LIMITS = {
    "xcresult-failure-ax.txt": 512 * 1024,
    "xcresult-failure-screenshot.png": 2 * 1024 * 1024,
}
MAX_ATTACHMENT_EXPORT_LOG_BYTES = 16 * 1024
RESULT_DIAGNOSTIC_TIMEOUT_SECONDS = 240
COMMAND_TIMEOUT_SECONDS = 30 * 60
TOTAL_RUN_BUDGET_SECONDS = 43 * 60
FIXTURE_BUILD_TIMEOUT_SECONDS = 25 * 60
SERVER_READY_TIMEOUT_SECONDS = 45
TASK_FIXTURE_READY_TIMEOUT_SECONDS = 45
TVOS_BOOT_TIMEOUT_SECONDS = 300
TASK_FIXTURE_PRODUCT = "task-watch-ui-fixture"
TASK_FIXTURE_TARGET = "TaskWatchUIFixture"
EXPECTED_UI_CASES = (
    "testAutomaticTaskWatchRecoversWithoutResubmission",
    "testManualTaskWatchReconnectPreservesPlayback",
)
EXPECTED_CASES_BY_PLATFORM = {
    "macos": EXPECTED_UI_CASES + ("testConnectBrowseAndControlRealPlayback",),
    "tvos": EXPECTED_UI_CASES + ("testRemoteConnectPageSelectAndPlayRealFixture",),
}
EXPECTED_TASK_IDS = {
    "automatic": "ui-e2e-automatic",
    "manual": "ui-e2e-manual",
}


class UIE2EError(RuntimeError):
    pass


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    bytes_written: int
    timed_out: bool = False
    output_limited: bool = False


@dataclass(frozen=True)
class PlatformConfig:
    scheme: str
    test_target: str


PLATFORMS = {
    "macos": PlatformConfig("MacOSNetPlayerUIE2E", "MacOSNetPlayerUITests"),
    "tvos": PlatformConfig("TVOSNetPlayerUIE2E", "TVOSNetPlayerUITests"),
}


_LINUX_PROC_ROOT = Path("/proc")


def _linux_group_has_live_members(pgid: int) -> bool:
    """Observe this PGID without mistaking unreaped zombies for live members."""
    observed_member = False
    deadline = time.monotonic() + 1
    with os.scandir(_LINUX_PROC_ROOT) as entries:
        for count, entry in enumerate(entries, start=1):
            if count > 4096 or time.monotonic() >= deadline:
                raise UIE2EError(
                    "Cleanup unverified: Linux process query exceeded its "
                    "4096-entry/1-second cap."
                )
            if not re.fullmatch(r"[0-9]+", entry.name):
                continue
            try:
                with (Path(entry.path) / "stat").open("rb") as reader:
                    stat = reader.read(4097)
            except (FileNotFoundError, ProcessLookupError):
                continue  # The process disappeared during enumeration.
            if len(stat) > 4096:
                raise UIE2EError(
                    "Cleanup unverified: Linux process stat exceeds 4096 bytes."
                )
            # comm can contain spaces and closing parentheses. Fields following
            # the final ') ' are state, ppid, pgrp, sid, ... in /proc/PID/stat.
            prefix, separator, suffix = stat.rpartition(b") ")
            fields = suffix.split()
            try:
                if (
                    not separator
                    or not prefix.startswith(entry.name.encode("ascii") + b" (")
                    or len(fields) < 3
                    or len(fields[0]) != 1
                ):
                    raise ValueError("missing process stat fields")
                member_pgid = int(fields[2])
            except ValueError as error:
                raise UIE2EError(
                    "Cleanup unverified: malformed Linux process stat."
                ) from error
            if member_pgid == pgid:
                observed_member = True
                if fields[0] not in (b"Z", b"X", b"x"):
                    return True
    if observed_member:
        return False
    # An empty/hidden /proc view is not proof of termination when the group
    # still exists. Recheck only for disappearance, never send a signal here.
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return False
    raise UIE2EError("Cleanup unverified: Linux process group has no visible members.")


def _group_has_live_members(pgid: int) -> bool:
    if sys.platform == "darwin":
        # Darwin killpg reports EPERM for zombie-only/empty groups. Query only
        # this PGID through libproc; a full buffer or denied query is unverified.
        library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        query = library.proc_listpgrppids
        query.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
        query.restype = ctypes.c_int
        pids = (ctypes.c_int * 4096)()
        ctypes.set_errno(0)
        result = query(pgid, pids, ctypes.sizeof(pids))
        error = ctypes.get_errno()
        if result < 0 or (result == 0 and error):
            raise OSError(error, "Could not inspect the owned process group")
        if all(pids):
            raise UIE2EError("Cleanup unverified: owned process group exceeds the 4096-member query cap.")
        for pid in pids:
            if not pid:
                continue
            try:
                if os.getpgid(pid) == pgid:
                    return True
            except ProcessLookupError:
                continue  # A zombie or member that already exited.
        return False
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return False
    if sys.platform.startswith("linux"):
        return _linux_group_has_live_members(pgid)
    return True


def _signal_process_group(process: subprocess.Popen[bytes], signum: int) -> None:
    try:
        os.killpg(process.pid, signum)
    except ProcessLookupError:
        pass
    except PermissionError:
        if _group_has_live_members(process.pid):
            raise


def _child_has_exited(process: subprocess.Popen[bytes]) -> bool:
    if process.returncode is not None:
        return True
    if not hasattr(os, "waitid") or not hasattr(os, "WNOWAIT"):
        raise UIE2EError("Cannot reserve the owned process-group ID without waitid(WNOWAIT).")
    try:
        status = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    except ChildProcessError as error:
        if process.returncode is not None:
            return True
        raise UIE2EError("Owned child was reaped externally; process-group identity is unverified.") from error
    return status is not None and status.si_pid == process.pid


def stop_process_group(
    process: subprocess.Popen[bytes], grace_seconds: float = 2.0
) -> None:
    if process.returncode is not None:
        if not _group_has_live_members(process.pid):
            return
        raise UIE2EError(
            "Cleanup unverified: leader already reaped; refusing to signal an unreserved PGID."
        )
    # Keep the direct child unreaped until all signals are dispatched. Its PID
    # reserves our start_new_session PGID against reuse by an unrelated group.
    if not _child_has_exited(process):
        try:
            matches = os.getpgid(process.pid) == process.pid and os.getsid(process.pid) == process.pid
        except ProcessLookupError:
            # Darwin does not expose group/session metadata for a zombie. A
            # WNOWAIT observation still proves our unreaped child reserves PID.
            matches = _child_has_exited(process)
        if not matches:
            raise UIE2EError("Cleanup unverified: owned child is not its original session/group leader.")
    _signal_process_group(process, signal.SIGTERM)
    deadline = time.monotonic() + grace_seconds
    while not _child_has_exited(process) and time.monotonic() < deadline:
        time.sleep(0.02)
    _signal_process_group(process, signal.SIGKILL)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired as error:
        raise UIE2EError("Could not reap an owned child process group.") from error
    # After reaping, never send another signal: a newly reused PGID would no
    # longer be anchored by our child. Only observe bounded group disappearance.
    deadline = time.monotonic() + 2
    while True:
        if not _group_has_live_members(process.pid):
            return
        if time.monotonic() >= deadline:
            raise UIE2EError("Cleanup unverified: process group still exists after leader reaping.")
        time.sleep(0.02)


def run_process(
    argv: list[str],
    *,
    log_path: Path,
    timeout_seconds: float,
    max_output_bytes: int,
    env: dict[str, str] | None = None,
    cwd: Path | None = None,
) -> CommandResult:
    """Run one owned process group with a hard deadline and retained-byte limit."""
    if timeout_seconds <= 0 or max_output_bytes <= 0:
        raise ValueError("Process deadline and output limit must be positive.")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    selector = selectors.DefaultSelector()
    process: subprocess.Popen[bytes] | None = None
    bytes_written = 0
    timed_out = False
    output_limited = False
    deadline = time.monotonic() + timeout_seconds

    try:
        with log_path.open("xb") as log_file:
            process = subprocess.Popen(
                argv,
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
                close_fds=True,
            )
            assert process.stdout is not None
            os.set_blocking(process.stdout.fileno(), False)
            selector.register(process.stdout, selectors.EVENT_READ)

            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    timed_out = True
                    break

                events = selector.select(min(0.2, remaining))
                for key, _ in events:
                    chunk = os.read(key.fd, 64 * 1024)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    available = max_output_bytes - bytes_written
                    if len(chunk) > available:
                        if available:
                            log_file.write(chunk[:available])
                            bytes_written += available
                        output_limited = True
                        break
                    log_file.write(chunk)
                    bytes_written += len(chunk)

                if output_limited:
                    break
                if _child_has_exited(process) and not selector.get_map():
                    break

            log_file.flush()
    finally:
        primary_error = sys.exc_info()[1]
        cleanup_errors: list[Exception] = []
        try:
            selector.close()
        except Exception as error:
            cleanup_errors.append(error)
        if process is not None:
            try:
                stop_process_group(process)
            except Exception as error:
                cleanup_errors.append(error)
            if process.stdout is not None:
                try:
                    process.stdout.close()
                except Exception as error:
                    cleanup_errors.append(error)
        if cleanup_errors:
            context = []
            if timed_out:
                context.append(f"Command exceeded its {timeout_seconds:g}-second deadline")
            if output_limited:
                context.append(f"Command exceeded its {max_output_bytes}-byte output limit")
            if primary_error is not None:
                context.append(
                    f"Command failed: {type(primary_error).__name__}: {str(primary_error)[:512]}"
                )
            if not context:
                context.append("Command process cleanup failed")
            command = json.dumps(argv, ensure_ascii=True)
            if len(command) > 4096:
                command = command[:4096] + " [argv truncated]"
            details = "; ".join(
                f"{type(error).__name__}: {str(error)[:512]}"
                for error in cleanup_errors
            )
            raise UIE2EError(
                "; ".join(context)
                + "; cleanup unverified; child reaping not confirmed: "
                + details
                + f"; argv={command}; pid={process.pid if process is not None else 'not-started'}"
                + f"; timeout_seconds={timeout_seconds:g}; max_output_bytes={max_output_bytes}"
                + f"; retained_bytes={bytes_written}; log={log_path}"
            ) from (primary_error if primary_error is not None else cleanup_errors[0])

    assert process is not None
    return CommandResult(
        returncode=process.wait(timeout=0),
        bytes_written=bytes_written,
        timed_out=timed_out,
        output_limited=output_limited,
    )


class CleanupStack:
    """Run all registered cleanups in reverse order, even after a failure."""

    def __init__(
        self, on_failure: Callable[[BaseException], None] | None = None
    ) -> None:
        self._callbacks: list[tuple[str, Callable[[], None]]] = []
        self.failures: list[str] = []
        self.on_failure = on_failure

    def defer(self, label: str, callback: Callable[[], None]) -> None:
        self._callbacks.append((label, callback))

    def close(self) -> None:
        callbacks, self._callbacks = self._callbacks, []
        for label, callback in reversed(callbacks):
            try:
                callback()
            except Exception as error:  # Cleanup must continue after one failure.
                self.failures.append(f"{label}: {error}")

    def __enter__(self) -> CleanupStack:
        return self

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> bool:
        if isinstance(exc, BaseException) and self.on_failure is not None:
            try:
                self.on_failure(exc)
            except Exception as error:
                self.failures.append(f"failure logging: {error}")
        self.close()
        if self.failures:
            message = "UI E2E cleanup failures: " + "; ".join(self.failures)
            if exc_type is None:
                raise UIE2EError(message)
            print(message, file=sys.stderr)
        return False


class BoundedOwnedProcess:
    """Own a long-running child and cap its combined stdout/stderr log."""

    def __init__(
        self,
        argv: list[str],
        *,
        log_path: Path,
        max_output_bytes: int,
        env: dict[str, str] | None = None,
    ) -> None:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        self.log_path = log_path
        self.max_output_bytes = max_output_bytes
        self.bytes_written = 0
        self.output_limited = False
        self._lock = threading.Lock()
        self._stop_lock = threading.Lock()
        self._log_file = log_path.open("xb")
        self.process = subprocess.Popen(
            argv,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            close_fds=True,
        )
        self._reader = threading.Thread(target=self._drain, daemon=True)
        self._reader.start()

    def _drain(self) -> None:
        assert self.process.stdout is not None
        while True:
            chunk = self.process.stdout.read(16 * 1024)
            if not chunk:
                break
            with self._lock:
                available = self.max_output_bytes - self.bytes_written
                if len(chunk) > available:
                    if available:
                        self._log_file.write(chunk[:available])
                        self.bytes_written += available
                    self.output_limited = True
                else:
                    self._log_file.write(chunk)
                    self.bytes_written += len(chunk)
                self._log_file.flush()
                limited = self.output_limited
            if limited:
                with self._stop_lock:
                    stop_process_group(self.process)
                break

    def stop(self) -> None:
        with self._stop_lock:
            stop_process_group(self.process)
        self._reader.join(timeout=5)
        if self._reader.is_alive():
            if self.process.stdout is not None:
                self.process.stdout.close()
            self._reader.join(timeout=2)
        self._log_file.close()
        if self._reader.is_alive():
            raise UIE2EError("Owned server output reader did not stop.")

    def is_running(self) -> bool:
        return not _child_has_exited(self.process)


def _require_result(result: CommandResult, description: str, log_path: Path) -> None:
    if result.timed_out:
        raise UIE2EError(f"{description} exceeded its deadline; log: {log_path}")
    if result.output_limited:
        raise UIE2EError(f"{description} exceeded its output limit; log: {log_path}")
    if result.returncode != 0:
        raise UIE2EError(
            f"{description} exited {result.returncode}; log: {log_path}"
        )


def run_capture(
    argv: list[str],
    *,
    logs_dir: Path,
    name: str,
    timeout_seconds: float = 30,
    max_output_bytes: int = 64 * 1024,
    env: dict[str, str] | None = None,
    cwd: Path | None = None,
) -> str:
    log_path = logs_dir / f"{name}.log"
    result = run_process(
        argv,
        log_path=log_path,
        timeout_seconds=timeout_seconds,
        max_output_bytes=max_output_bytes,
        env=env,
        cwd=cwd,
    )
    _require_result(result, name, log_path)
    return log_path.read_text(encoding="utf-8", errors="replace")


def validate_server_address(value: str | None) -> str:
    if not value:
        raise UIE2EError("UI_E2E_SERVER_ADDRESS must not be empty.")
    match = re.fullmatch(r"http://127\.0\.0\.1:([1-9][0-9]{0,4})", value)
    if not match or int(match.group(1)) > 65535:
        raise UIE2EError(
            "UI_E2E_SERVER_ADDRESS must be an http://127.0.0.1:<port> endpoint."
        )
    return value


def verify_scheme_contract(scheme_path: Path, expected_target: str) -> None:
    if not scheme_path.is_file():
        raise UIE2EError(f"Required shared UI E2E scheme is missing: {scheme_path}")
    try:
        root = ET.parse(scheme_path).getroot()
    except ET.ParseError as error:
        raise UIE2EError(f"Could not parse UI E2E scheme: {scheme_path}") from error

    test_action = root.find("TestAction")
    if test_action is None:
        raise UIE2EError("UI E2E scheme has no TestAction.")
    if test_action.get("shouldUseLaunchSchemeArgsEnv") != "NO":
        raise UIE2EError(
            "UI E2E TestAction must disable scheme launch argument/environment inheritance."
        )

    endpoint_mapping = any(
        variable.get("key") == "UI_E2E_SERVER_ADDRESS"
        and variable.get("value") == "$(UI_E2E_SERVER_ADDRESS)"
        and variable.get("isEnabled", "YES").upper() == "YES"
        for variable in test_action.findall(
            "./EnvironmentVariables/EnvironmentVariable"
        )
    )
    if not endpoint_mapping:
        raise UIE2EError(
            "UI E2E TestAction must explicitly map "
            "UI_E2E_SERVER_ADDRESS=$(UI_E2E_SERVER_ADDRESS)."
        )

    task_endpoint_mapping = any(
        variable.get("key") == "UI_E2E_TASK_SERVER_ADDRESS"
        and variable.get("value") == "$(UI_E2E_TASK_SERVER_ADDRESS)"
        and variable.get("isEnabled", "YES").upper() == "YES"
        for variable in test_action.findall(
            "./EnvironmentVariables/EnvironmentVariable"
        )
    )
    if not task_endpoint_mapping:
        raise UIE2EError(
            "UI E2E TestAction must explicitly map "
            "UI_E2E_TASK_SERVER_ADDRESS=$(UI_E2E_TASK_SERVER_ADDRESS)."
        )

    test_target_present = any(
        testable.get("skipped", "NO").upper() != "YES"
        and reference.get("BlueprintName") == expected_target
        for testable in test_action.findall("./Testables/TestableReference")
        for reference in testable.findall("./BuildableReference")
    )
    if not test_target_present:
        raise UIE2EError(
            f"UI E2E scheme does not test the expected target {expected_target}."
        )


def build_xcodebuild_command(
    *,
    platform: str,
    server_address: str | None,
    task_server_address: str | None = None,
    app_bundle_identifier: str,
    derived_data_path: Path,
    result_bundle_path: Path,
    destination: str,
) -> list[str]:
    endpoint = validate_server_address(server_address)
    task_endpoint = validate_server_address(task_server_address) if task_server_address else None
    if not re.fullmatch(
        r"dev\.joey\.tvos-net-player\.uie2e\.[0-9a-f]{32}",
        app_bundle_identifier,
    ):
        raise UIE2EError("APP_BUNDLE_IDENTIFIER must be unique to this UI E2E run.")
    config = PLATFORMS[platform]
    signing_settings = (
        [
            "CODE_SIGN_IDENTITY=-",
            "CODE_SIGNING_ALLOWED=YES",
            "CODE_SIGN_STYLE=Manual",
            "DEVELOPMENT_TEAM=",
        ]
        if platform == "macos"
        else ["CODE_SIGNING_ALLOWED=NO"]
    )
    return [
        "xcodebuild",
        "-quiet",
        "-onlyUsePackageVersionsFromResolvedFile",
        "-skipPackagePluginValidation",
        "-project",
        str(ROOT / "TVOSNetPlayer.xcodeproj"),
        "-scheme",
        config.scheme,
        "-configuration",
        "Debug",
        "-destination",
        destination,
        "-derivedDataPath",
        str(derived_data_path),
        "-resultBundlePath",
        str(result_bundle_path),
        "-parallel-testing-enabled",
        "NO",
        *signing_settings,
        f"APP_BUNDLE_IDENTIFIER={app_bundle_identifier}",
        f"UI_E2E_SERVER_ADDRESS={endpoint}",
        *([f"UI_E2E_TASK_SERVER_ADDRESS={task_endpoint}"] if task_endpoint else []),
        "test",
    ]


def build_task_fixture_command(
    executable: Path,
    *,
    ready_file: Path,
    metrics_file: Path,
    media_url: str,
) -> list[str]:
    validate_media_fixture_url(media_url)
    return [
        str(executable),
        "--ready-file",
        str(ready_file),
        "--metrics-file",
        str(metrics_file),
        "--media-url",
        media_url,
    ]


def validate_media_fixture_url(value: str) -> str:
    try:
        parsed = urllib.parse.urlsplit(value)
        port = parsed.port
    except ValueError as error:
        raise UIE2EError("Task fixture media URL is malformed.") from error
    if (
        parsed.scheme != "http"
        or parsed.hostname != "127.0.0.1"
        or port is None
        or not 1 <= port <= 65535
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or not re.fullmatch(r"/media/local\.default\.[A-Za-z0-9_-]+/original", parsed.path)
    ):
        raise UIE2EError(
            "Task fixture media URL must be a loopback Rust cache /media/<fixture>/original URL."
        )
    return value


def build_rust_fixture_media_url(media_port: int) -> str:
    if not 1 <= media_port <= 65535:
        raise UIE2EError("Rust cache media port is invalid.")
    relative_path = "Fixture 055.mp4"
    encoded_path = base64.urlsafe_b64encode(relative_path.encode("utf-8")).decode("ascii").rstrip("=")
    item_id = f"local.default.{encoded_path}"
    return validate_media_fixture_url(
        f"http://127.0.0.1:{media_port}/media/{urllib.parse.quote(item_id, safe='')}/original"
    )


def verify_media_fixture_url(media_url: str, expected_size: int) -> None:
    validate_media_fixture_url(media_url)
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}),
        _NoRedirectHandler(),
    )
    request = urllib.request.Request(media_url, method="HEAD")
    try:
        with opener.open(request, timeout=3) as response:
            if (
                response.status != 200
                or response.headers.get_content_type() != "video/mp4"
                or response.headers.get("Content-Length") != str(expected_size)
            ):
                raise UIE2EError(
                    "Rust cache fixture URL did not return the expected MP4 HEAD response."
                )
    except (OSError, urllib.error.URLError) as error:
        raise UIE2EError("Rust cache fixture media endpoint is not ready.") from error


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request: object, response: object, code: int, message: str,
                         headers: object, new_url: str) -> None:
        return None


def build_task_fixture_swift_command() -> list[str]:
    return [
        "swift",
        "build",
        "--product",
        TASK_FIXTURE_PRODUCT,
        "--configuration",
        "debug",
        "--disable-automatic-resolution",
    ]


def resolve_swift_bin_path(logs_dir: Path) -> Path:
    output = run_capture(
        ["swift", "build", "--show-bin-path", "--disable-automatic-resolution"],
        logs_dir=logs_dir,
        name="task-fixture-bin-path",
        timeout_seconds=30,
        max_output_bytes=16 * 1024,
        cwd=ROOT,
    )
    lines = [line.strip() for line in output.splitlines() if line.strip()]
    if not lines:
        raise UIE2EError("SwiftPM did not report a fixture binary directory.")
    bin_path = Path(lines[-1]).resolve()
    build_path = (ROOT / ".build").resolve()
    if not bin_path.is_relative_to(build_path):
        raise UIE2EError("Swift fixture binary path is outside the reusable package .build directory.")
    return bin_path


def build_task_fixture(logs_dir: Path, *, timeout_seconds: float) -> Path:
    result = run_process(
        build_task_fixture_swift_command(),
        log_path=logs_dir / "task-fixture-build.log",
        timeout_seconds=timeout_seconds,
        max_output_bytes=MAX_COMMAND_LOG_BYTES,
        cwd=ROOT,
    )
    _require_result(result, "TaskWatchUIFixture SwiftPM debug build", logs_dir / "task-fixture-build.log")
    binary = resolve_swift_bin_path(logs_dir) / TASK_FIXTURE_PRODUCT
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise UIE2EError("SwiftPM task fixture product is missing or not executable.")
    return binary


def _read_bounded_json(path: Path, *, description: str) -> object:
    try:
        if path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_FIXTURE_JSON_BYTES:
            raise UIE2EError(f"{description} is missing, non-regular, or exceeds its size cap.")
        with path.open("rb") as source:
            payload = source.read(MAX_FIXTURE_JSON_BYTES + 1)
        if len(payload) > MAX_FIXTURE_JSON_BYTES:
            raise UIE2EError(f"{description} exceeds its size cap.")
        return json.loads(payload)
    except (OSError, json.JSONDecodeError) as error:
        raise UIE2EError(f"Could not read {description} JSON.") from error


def wait_for_task_fixture(
    fixture: BoundedOwnedProcess,
    ready_file: Path,
    *,
    timeout_seconds: float = TASK_FIXTURE_READY_TIMEOUT_SECONDS,
) -> str:
    deadline = time.monotonic() + timeout_seconds
    last_error = "readiness file has not been created"
    while time.monotonic() < deadline:
        if fixture.output_limited:
            raise UIE2EError("Task fixture exceeded its bounded log size.")
        if not fixture.is_running():
            raise UIE2EError("Owned task fixture exited before becoming ready.")
        if ready_file.exists():
            payload = _read_bounded_json(ready_file, description="Task fixture readiness")
            if (
                isinstance(payload, dict)
                and type(payload.get("schema_version")) is int
                and payload.get("schema_version") == 1
            ):
                address = payload.get("server_address")
                if isinstance(address, str) and re.fullmatch(
                    r"127\.0\.0\.1:[1-9][0-9]{0,4}", address
                ) and int(address.rsplit(":", 1)[1]) <= 65535:
                    return validate_server_address(f"http://{address}")
            last_error = "readiness JSON has an unexpected schema or address"
        time.sleep(0.1)
    raise UIE2EError(f"Task fixture readiness timed out: {last_error}")


def validate_task_fixture_metrics(payload: object) -> dict[str, object]:
    if (
        not isinstance(payload, dict)
        or type(payload.get("schema_version")) is not int
        or payload.get("schema_version") != 1
    ):
        raise UIE2EError("Task fixture metrics have an unsupported schema.")
    scenarios = payload.get("scenarios")
    if not isinstance(scenarios, dict):
        raise UIE2EError("Task fixture metrics are missing scenario counters.")
    validated: dict[str, object] = {}
    for scenario in ("automatic", "manual"):
        counters = scenarios.get(scenario)
        if not isinstance(counters, dict):
            raise UIE2EError(f"Task fixture metrics are missing the {scenario} scenario.")
        if counters.get("task_id") != EXPECTED_TASK_IDS[scenario]:
            raise UIE2EError(f"Task fixture {scenario} scenario returned an unexpected task ID.")
        if type(counters.get("create_count")) is not int or counters.get("create_count") != 1:
            raise UIE2EError(f"Task fixture {scenario} scenario must create exactly one task.")
        for key in ("watch_count", "get_task_count", "output_count"):
            value = counters.get(key)
            if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
                raise UIE2EError(f"Task fixture {scenario} scenario requires positive {key}.")
        cancel_count = counters.get("cancel_count")
        if type(cancel_count) is not int or cancel_count != 0:
            raise UIE2EError(f"Task fixture {scenario} scenario must not cancel server work.")
        validated[scenario] = dict(counters)
    manual = scenarios["manual"]
    if manual.get("manual_resume_read_before_watch") is not True:
        raise UIE2EError(
            "Manual task fixture did not confirm GetTask after retry exhaustion and before its final watch."
        )
    return {"schema_version": 1, "scenarios": validated}


def allocate_loopback_ports() -> tuple[int, int]:
    sockets: list[socket.socket] = []
    try:
        for _ in range(8):
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.bind(("127.0.0.1", 0))
            sockets.append(sock)
        ports = [sock.getsockname()[1] for sock in sockets]
        if len(set(ports)) == len(ports):
            return ports[0], ports[1]
    finally:
        for sock in sockets:
            sock.close()
    raise UIE2EError("Could not reserve two distinct loopback ports.")


def resolve_server_binary() -> Path:
    configured = os.environ.get("CACHE_SERVER_BINARY")
    binary = Path(configured).expanduser() if configured else (
        ROOT / "target" / "release" / SERVER_NAME
    )
    binary = binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise UIE2EError(
            "Rust cache server binary is missing or not executable. Set "
            "CACHE_SERVER_BINARY to an existing built binary."
        )
    return binary


def create_fixture_media(work_dir: Path, logs_dir: Path) -> Path:
    ffmpeg = shutil.which("ffmpeg")
    ffprobe = shutil.which("ffprobe")
    if not ffmpeg or not ffprobe:
        raise UIE2EError("Both ffmpeg and ffprobe must already be installed.")

    cache_dir = work_dir / "Cache"
    cache_dir.mkdir(mode=0o700)
    seed = work_dir / "fixture-seed.mp4"
    run_capture(
        [
            ffmpeg,
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=4",
            "-t",
            str(FIXTURE_DURATION_SECONDS),
            "-an",
            "-c:v",
            "libx264",
            "-profile:v",
            "baseline",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "ultrafast",
            "-crf",
            "38",
            "-movflags",
            "+faststart",
            str(seed),
        ],
        logs_dir=logs_dir,
        name="ffmpeg-fixture",
        timeout_seconds=120,
        max_output_bytes=64 * 1024,
    )

    probe_text = run_capture(
        [
            ffprobe,
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type,pix_fmt",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
            str(seed),
        ],
        logs_dir=logs_dir,
        name="ffprobe-fixture",
        timeout_seconds=15,
        max_output_bytes=64 * 1024,
    )
    try:
        probe = json.loads(probe_text)
        streams = probe["streams"]
        video = [item for item in streams if item.get("codec_type") == "video"]
        duration = float(probe["format"]["duration"])
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise UIE2EError("ffprobe returned an invalid fixture description.") from error
    if (
        len(video) != 1
        or video[0].get("codec_name") != "h264"
        or video[0].get("pix_fmt") != "yuv420p"
        or duration < 90
        or any(item.get("codec_type") == "audio" for item in streams)
    ):
        raise UIE2EError(
            "Fixture must contain one >=90-second H.264/yuv420p video stream and no audio."
        )

    seed_size = seed.stat().st_size
    if seed_size == 0 or seed_size * FIXTURE_COUNT > MAX_FIXTURE_BYTES:
        raise UIE2EError("Generated fixture exceeds the private media size budget.")

    for index in range(1, FIXTURE_COUNT + 1):
        shutil.copyfile(seed, cache_dir / f"Fixture {index:03}.mp4")
    seed.unlink()
    fixtures = list(cache_dir.glob("Fixture [0-9][0-9][0-9].mp4"))
    if len(fixtures) != FIXTURE_COUNT:
        raise UIE2EError("Fixture generation did not produce exactly 55 MP4 files.")
    return cache_dir


def build_server_argv(
    binary: Path,
    *,
    cache_dir: Path,
    state_dir: Path,
    grpc_port: int,
    media_port: int,
) -> list[str]:
    if grpc_port == media_port:
        raise UIE2EError("gRPC and media ports must be distinct.")
    return [
        str(binary),
        "--Cache:ServerName",
        "UI E2E Cache",
        "--Cache:RootPath",
        str(cache_dir),
        "--Cache:TaskStatePath",
        str(state_dir / "tasks.json"),
        "--Cache:GrpcListenUrl",
        f"http://127.0.0.1:{grpc_port}",
        "--Cache:MediaListenUrl",
        f"http://127.0.0.1:{media_port}",
        "--Cache:BonjourEnabled",
        "false",
        "--Cache:AllowBilibiliLoginSessions",
        "false",
        "--Cache:BilibiliWorkerEnabled",
        "false",
    ]


def wait_for_server(
    server: BoundedOwnedProcess,
    *,
    grpc_port: int,
    media_port: int,
    timeout_seconds: float = SERVER_READY_TIMEOUT_SECONDS,
) -> None:
    media_url = f"http://127.0.0.1:{media_port}/"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + timeout_seconds
    last_error = "not ready"
    while time.monotonic() < deadline:
        if server.output_limited:
            raise UIE2EError("Cache server exceeded its bounded log size.")
        if not server.is_running():
            raise UIE2EError("Owned cache server exited before becoming ready.")
        try:
            with socket.create_connection(("127.0.0.1", grpc_port), timeout=0.5):
                pass
            request = urllib.request.Request(media_url, method="GET")
            with opener.open(request, timeout=1) as response:
                payload = json.loads(response.read(4096))
            if payload == {
                "service": "TVOSNetPlayer.CacheServer",
                "controlPlane": "gRPC",
                "mediaPlane": "HTTP",
            }:
                return
            last_error = "media endpoint returned an unexpected service identity"
        except (OSError, urllib.error.URLError, json.JSONDecodeError) as error:
            last_error = str(error)
        time.sleep(0.2)
    raise UIE2EError(f"Owned cache server readiness timed out: {last_error}")


def _version_tuple(value: str) -> tuple[int, ...]:
    match = re.findall(r"\d+", value)
    return tuple(int(part) for part in match) if match else (0,)


def _available_tvos_device(
    runtimes_payload: dict[str, object], devices_payload: dict[str, object]
) -> tuple[str, str]:
    runtimes = runtimes_payload.get("runtimes", [])
    if not isinstance(runtimes, list):
        raise UIE2EError("Simulator runtime inventory has an invalid format.")
    runtime_map = {
        item.get("identifier"): item
        for item in runtimes
        if isinstance(item, dict)
        and item.get("isAvailable") is True
        and "tvos" in str(item.get("identifier", "")).lower()
    }
    devices = devices_payload.get("devices", {})
    if not isinstance(devices, dict):
        raise UIE2EError("Simulator device inventory has an invalid format.")

    candidates: list[tuple[tuple[int, ...], str, str]] = []
    for runtime_id, runtime in runtime_map.items():
        runtime_devices = devices.get(runtime_id, [])
        if not isinstance(runtime_devices, list):
            continue
        for device in runtime_devices:
            if not isinstance(device, dict) or device.get("isAvailable") is not True:
                continue
            device_type = device.get("deviceTypeIdentifier")
            if isinstance(device_type, str) and device_type:
                version = str(runtime.get("version", runtime_id))
                candidates.append((_version_tuple(version), runtime_id, device_type))
    if not candidates:
        raise UIE2EError(
            "No installed, available tvOS simulator runtime/device was found; "
            "runtime downloads are disabled."
        )
    _, runtime_id, device_type = max(candidates)
    return runtime_id, device_type


def _read_json_file(path: Path) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise UIE2EError(f"Could not read JSON output from {path.name}.") from error


def _list_simulators(logs_dir: Path) -> dict[str, object]:
    output = run_capture(
        ["xcrun", "simctl", "list", "devices", "--json"],
        logs_dir=logs_dir,
        name=f"simctl-devices-{uuid.uuid4().hex[:8]}",
        timeout_seconds=30,
        max_output_bytes=1024 * 1024,
    )
    try:
        payload = json.loads(output)
    except json.JSONDecodeError as error:
        raise UIE2EError("simctl returned invalid device inventory JSON.") from error
    if not isinstance(payload, dict):
        raise UIE2EError("simctl returned invalid device inventory JSON.")
    return payload


def _find_device_by_name(
    payload: dict[str, object], name: str
) -> list[tuple[str, str]]:
    devices = payload.get("devices", {})
    matches: list[tuple[str, str]] = []
    if isinstance(devices, dict):
        for runtime_id, items in devices.items():
            if not isinstance(items, list):
                continue
            for item in items:
                if isinstance(item, dict) and item.get("name") == name:
                    matches.append((str(runtime_id), str(item.get("udid", ""))))
    return matches


def create_owned_tvos_simulator(
    *, logs_dir: Path, cleanup: CleanupStack
) -> str:
    runtimes_text = run_capture(
        ["xcrun", "simctl", "list", "runtimes", "--json"],
        logs_dir=logs_dir,
        name="simctl-runtimes",
        timeout_seconds=30,
        max_output_bytes=256 * 1024,
    )
    devices_text = run_capture(
        ["xcrun", "simctl", "list", "devices", "available", "--json"],
        logs_dir=logs_dir,
        name="simctl-available-devices",
        timeout_seconds=30,
        max_output_bytes=1024 * 1024,
    )
    try:
        runtimes = json.loads(runtimes_text)
        devices = json.loads(devices_text)
    except json.JSONDecodeError as error:
        raise UIE2EError("Could not parse installed simulator inventory.") from error
    if not isinstance(runtimes, dict) or not isinstance(devices, dict):
        raise UIE2EError("Could not parse installed simulator inventory.")
    runtime_id, device_type = _available_tvos_device(runtimes, devices)
    name = f"TVOSNetPlayer-UIE2E-{uuid.uuid4().hex}"
    create_log = logs_dir / "simctl-create.log"
    create_error: UIE2EError | None = None
    try:
        output = run_capture(
            ["xcrun", "simctl", "create", name, device_type, runtime_id],
            logs_dir=logs_dir,
            name="simctl-create",
            timeout_seconds=60,
            max_output_bytes=16 * 1024,
        )
        match = re.search(
            r"\b[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-"
            r"[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}\b",
            output,
        )
        udid = match.group(0) if match else ""
    except UIE2EError as error:
        create_error = error
        udid = ""

    if not udid:
        matches = _find_device_by_name(_list_simulators(logs_dir), name)
        if len(matches) == 1:
            udid = matches[0][1]
        elif matches:
            raise UIE2EError(
                "Simulator creation produced an ambiguous owned-device name; "
                "no simulator was deleted."
            ) from create_error
        elif create_error is not None:
            raise create_error
        else:
            raise UIE2EError(
                f"simctl did not return a device identifier; log: {create_log}"
            )

    if not re.fullmatch(
        r"[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-"
        r"[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}",
        udid,
    ):
        raise UIE2EError("simctl returned an invalid simulator identifier.")

    cleanup.defer(
        "owned tvOS simulator",
        lambda: delete_owned_simulator(name=name, udid=udid, logs_dir=logs_dir),
    )
    run_capture(
        ["xcrun", "simctl", "boot", udid],
        logs_dir=logs_dir,
        name="simctl-boot",
        timeout_seconds=60,
        max_output_bytes=32 * 1024,
    )
    run_capture(
        ["xcrun", "simctl", "bootstatus", udid, "-b"],
        logs_dir=logs_dir,
        name="simctl-bootstatus",
        timeout_seconds=TVOS_BOOT_TIMEOUT_SECONDS,
        max_output_bytes=128 * 1024,
    )
    return udid


def delete_owned_simulator(*, name: str, udid: str, logs_dir: Path) -> None:
    inventory = _list_simulators(logs_dir)
    matches = _find_device_by_name(inventory, name)
    if not matches:
        return
    if len(matches) != 1 or matches[0][1] != udid:
        raise UIE2EError(
            "Owned simulator identity changed; refusing to delete any device."
        )
    run_capture(
        ["xcrun", "simctl", "delete", udid],
        logs_dir=logs_dir,
        name="simctl-delete",
        timeout_seconds=60,
        max_output_bytes=32 * 1024,
    )
    if _find_device_by_name(_list_simulators(logs_dir), name):
        raise UIE2EError("Owned simulator remained after simctl delete.")


def _record_tool_inventory(platform: str, logs_dir: Path, runner_log: Path) -> None:
    commands = [
        ("xcode-version", ["xcodebuild", "-version"]),
        ("xcode-sdks", ["xcodebuild", "-showsdks"]),
    ]
    if platform == "tvos":
        commands.extend(
            [
                ("simctl-runtimes-inventory", ["xcrun", "simctl", "list", "runtimes", "--json"]),
                ("simctl-devices-inventory", ["xcrun", "simctl", "list", "devices", "available", "--json"]),
            ]
        )
    simulator_inventory: dict[str, dict[str, object]] = {}
    with runner_log.open("a", encoding="utf-8") as output:
        for name, argv in commands:
            text = run_capture(
                argv,
                logs_dir=logs_dir,
                name=name,
                timeout_seconds=30,
                max_output_bytes=512 * 1024,
            )
            if "runtimes-inventory" in name or "devices-inventory" in name:
                payload = json.loads(text)
                if not isinstance(payload, dict):
                    raise UIE2EError("Simulator inventory is not a JSON object.")
                simulator_inventory[name] = payload
                if "runtimes-inventory" in name:
                    listed: Iterable[object] = payload.get("runtimes", [])
                    compact = [
                        {
                            "name": item.get("name"),
                            "version": item.get("version"),
                            "available": item.get("isAvailable"),
                        }
                        for item in listed
                        if isinstance(item, dict)
                    ]
                else:
                    compact = []
                    for runtime, items in payload.get("devices", {}).items():
                        if not isinstance(items, list):
                            continue
                        for item in items:
                            if isinstance(item, dict) and item.get("isAvailable"):
                                compact.append(
                                    {
                                        "runtime": runtime,
                                        "name": item.get("name"),
                                        "deviceTypeIdentifier": item.get(
                                            "deviceTypeIdentifier"
                                        ),
                                    }
                                )
                output.write(
                    f"{name}: {json.dumps(compact, sort_keys=True)}\n"
                )
            else:
                output.write(f"{name}: {text.strip()}\n")
        output.flush()
    if platform == "tvos":
        _available_tvos_device(
            simulator_inventory["simctl-runtimes-inventory"],
            simulator_inventory["simctl-devices-inventory"],
        )


def mute_system_audio(logs_dir: Path) -> None:
    if sys.platform != "darwin":
        raise UIE2EError("UI playback E2E requires macOS system audio controls.")
    run_capture(
        ["osascript", "-e", "set volume output muted true"],
        logs_dir=logs_dir,
        name="system-mute-set",
        timeout_seconds=10,
        max_output_bytes=4096,
    )
    state = run_capture(
        ["osascript", "-e", "output muted of (get volume settings)"],
        logs_dir=logs_dir,
        name="system-mute-verify",
        timeout_seconds=10,
        max_output_bytes=4096,
    ).strip()
    if state.lower() != "true":
        raise UIE2EError("Could not verify macOS system output is muted.")


def _write_json(path: Path, payload: object) -> None:
    path.write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def build_xcresulttool_command(kind: str, result_bundle: Path) -> list[str]:
    if kind not in ("summary", "tests"):
        raise ValueError(f"Unsupported xcresulttool test-results command: {kind}")
    return [
        "xcrun",
        "xcresulttool",
        "get",
        "test-results",
        kind,
        "--path",
        str(result_bundle),
    ]


def _all_dicts(payload: object) -> Iterable[dict[str, object]]:
    if isinstance(payload, dict):
        yield payload
        for value in payload.values():
            yield from _all_dicts(value)
    elif isinstance(payload, list):
        for value in payload:
            yield from _all_dicts(value)


def _node_children(node: dict[str, object]) -> list[object]:
    for key in ("children", "testNodes", "nodes", "subtests"):
        value = node.get(key)
        if isinstance(value, list):
            return value
    return []


def _normalized_target_name(value: object) -> str:
    if not isinstance(value, str):
        return ""
    name = value.rstrip("/").rsplit("/", 1)[-1].strip()
    if name.endswith(".xctest"):
        name = name[: -len(".xctest")]
    return name


def _is_expected_target(node: dict[str, object], expected_target: str) -> bool:
    node_type = str(node.get("nodeType", "")).strip().lower()
    if node_type != "ui test bundle":
        return False
    return any(
        _normalized_target_name(node.get(key)) == expected_target
        for key in ("name", "nodeIdentifier", "nodeIdentifierURL", "identifier")
    )


def _test_status_counts(node: object) -> dict[str, int]:
    counts = {"passed": 0, "failed": 0, "skipped": 0}
    if isinstance(node, list):
        for item in node:
            for status, value in _test_status_counts(item).items():
                counts[status] += value
        return counts
    if not isinstance(node, dict):
        return counts

    children = _node_children(node)
    node_type = str(node.get("nodeType", "")).lower()
    name = str(node.get("name", ""))
    result = str(node.get("result", "")).lower()
    is_case = "case" in node_type or "method" in node_type or (
        not node_type and name.startswith("test")
    )
    nested_runs = any(
        "case" in str(child.get("nodeType", "")).lower() and child.get("result")
        for child in _all_dicts(children)
    )
    if children and (not is_case or nested_runs):
        for child in children:
            for status, value in _test_status_counts(child).items():
                counts[status] += value
        return counts

    if result and is_case:
        if result in ("passed", "success"):
            counts["passed"] += 1
        elif result in ("skipped", "expected failure"):
            counts["skipped"] += 1
        else:
            counts["failed"] += 1
    return counts


def validate_xcresult(
    summary: object,
    tests: object,
    expected_target: str,
    expected_cases: tuple[str, ...] | None = None,
) -> dict[str, int]:
    if not isinstance(summary, dict):
        raise UIE2EError("xcresult summary is not a JSON object.")
    required_counts = ("totalTestCount", "passedTests", "failedTests", "skippedTests")
    try:
        overall = {key: int(summary[key]) for key in required_counts}
    except (KeyError, TypeError, ValueError) as error:
        raise UIE2EError("xcresult summary is missing execution counts.") from error
    if overall["totalTestCount"] <= 0 or overall["passedTests"] <= 0:
        raise UIE2EError("xcresult proves no UI test actually passed.")
    if overall["failedTests"] != 0:
        raise UIE2EError("xcresult reports failed UI tests.")

    matching = [
        node
        for node in _all_dicts(tests)
        if _is_expected_target(node, expected_target)
    ]
    if not matching:
        raise UIE2EError(
            f"xcresult test tree does not contain expected UI target {expected_target}."
        )
    target_counts = {"passed": 0, "failed": 0, "skipped": 0}
    for node in matching:
        for status, value in _test_status_counts(node).items():
            target_counts[status] = max(target_counts[status], value)
    if target_counts["passed"] <= 0:
        raise UIE2EError(
            f"xcresult target {expected_target} has zero passed tests or is skipped-only."
        )
    if target_counts["failed"] > 0:
        raise UIE2EError(f"xcresult target {expected_target} contains failures.")
    if target_counts["skipped"] > 0 or overall["skippedTests"] > 0:
        raise UIE2EError("xcresult reports skipped UI tests; this gate requires none.")
    if expected_cases is not None:
        target_cases = [
            case
            for target in matching
            for case in _all_dicts(_node_children(target))
            if "case" in str(case.get("nodeType", "")).lower()
            or "method" in str(case.get("nodeType", "")).lower()
        ]
        for expected_case in expected_cases:
            matching_cases = [
                case
                for case in target_cases
                if re.search(
                    rf"(?<![A-Za-z0-9_]){re.escape(expected_case)}(?![A-Za-z0-9_])",
                    str(case.get("name", "")),
                )
            ]
            case_counts = {"passed": 0, "failed": 0, "skipped": 0}
            for case in matching_cases:
                for status, count in _test_status_counts(case).items():
                    case_counts[status] += count
            if not matching_cases or case_counts["passed"] <= 0:
                raise UIE2EError(
                    f"xcresult did not execute and pass required UI case {expected_case}."
                )
            if case_counts["failed"] or case_counts["skipped"]:
                raise UIE2EError(
                    f"xcresult required UI case {expected_case} failed or was skipped."
                )
    return overall | {f"target_{key}": value for key, value in target_counts.items()}


def _selected_failure_attachments(activities: object) -> list[dict[str, str]]:
    """Select only the named teardown AX/screenshot from this failed case."""
    selected: dict[str, dict[str, str]] = {}
    patterns = (
        (
            r"Failure accessibility tree(?:_\d{1,6}_[0-9A-Fa-f-]{36})?\.txt",
            "xcresult-failure-ax.txt",
        ),
        (
            r"Failure screenshot(?:_\d{1,6}_[0-9A-Fa-f-]{36})?\.png",
            "xcresult-failure-screenshot.png",
        ),
    )
    for activity in _all_dicts(activities):
        if activity.get("title") != "Tear Down":
            continue
        for node in _all_dicts(activity):
            attachments = node.get("attachments", [])
            if not isinstance(attachments, list):
                continue
            for attachment in attachments:
                if (
                    not isinstance(attachment, dict)
                    or attachment.get("lifetime") != "keepAlways"
                ):
                    continue
                name, payload_id = attachment.get("name"), attachment.get("payloadId")
                if not isinstance(name, str) or not isinstance(payload_id, str):
                    continue
                if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_~+=-]{0,511}", payload_id):
                    continue
                for pattern, filename in patterns:
                    if filename not in selected and re.fullmatch(pattern, name):
                        selected[filename] = {
                            "file": filename, "name": name, "payloadId": payload_id
                        }
    return [selected[name] for name in FAILURE_ATTACHMENT_LIMITS if name in selected]


def _file_limited_command(command: list[str], max_bytes: int) -> list[str]:
    # Set limits in a fresh interpreter, not preexec_fn in this multithreaded
    # runner. exec preserves our owned PID/session and the kernel file cap.
    launcher = (
        "import os,resource,sys; "
        "limit=int(sys.argv[1]); "
        "resource.setrlimit(resource.RLIMIT_CORE,(0,0)); "
        "resource.setrlimit(resource.RLIMIT_FSIZE,(limit,limit)); "
        "os.execvp(sys.argv[2],sys.argv[2:])"
    )
    return [sys.executable, "-c", launcher, str(max_bytes), *command]


def export_failure_attachments(
    activities: object,
    *,
    result_bundle: Path,
    logs_dir: Path,
    deadline: float,
    log: Callable[[str], None],
) -> None:
    selected = _selected_failure_attachments(activities)
    receipt: list[dict[str, object]] = []
    for attachment in selected:
        filename = attachment["file"]
        limit = FAILURE_ATTACHMENT_LIMITS[filename]
        item: dict[str, object] = dict(
            attachment, maxBytes=limit, status="unavailable"
        )
        receipt.append(item)
        try:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise UIE2EError("shared xcresult diagnostic deadline exhausted")
            with tempfile.TemporaryDirectory(
                prefix=".xcresult-attachment-", dir=logs_dir
            ) as directory:
                payload = Path(directory) / "payload"
                export_log = logs_dir / f"{filename}.export.log"
                command = [
                    "xcrun", "xcresulttool", "export", "object", "--legacy",
                    "--path", str(result_bundle), "--type", "file",
                    "--id", attachment["payloadId"], "--output-path", str(payload),
                ]
                captured = run_process(
                    _file_limited_command(command, limit),
                    log_path=export_log,
                    timeout_seconds=min(30, remaining),
                    max_output_bytes=MAX_ATTACHMENT_EXPORT_LOG_BYTES,
                    cwd=ROOT,
                )
                _require_result(captured, f"export {filename}", export_log)
                if payload.is_symlink() or not payload.is_file():
                    raise UIE2EError("export did not produce a regular payload file")
                size = payload.stat().st_size
                if not 0 < size <= limit:
                    raise UIE2EError(
                        f"payload byte cap exceeded or empty: {size} bytes, cap={limit}"
                    )
                if filename.endswith(".png"):
                    with payload.open("rb") as reader:
                        if reader.read(8) != b"\x89PNG\r\n\x1a\n":
                            raise UIE2EError("exported screenshot has no PNG signature")
                else:
                    payload.read_text(encoding="utf-8")
                payload.rename(logs_dir / filename)
                item.update(status="exported", bytes=size)
                log(
                    f"xcresult attachment exported: {filename} "
                    f"({size} bytes, cap={limit})."
                )
        except Exception as error:
            item["error"] = f"{type(error).__name__}: {' '.join(str(error).split())[:512]}"
            log(f"xcresult attachment {filename} unavailable: {item['error']}")
    if not selected:
        log(
            "xcresult attachments unavailable: no eligible named teardown "
            "AX/screenshot in first failed case metadata."
        )
    _write_json(
        logs_dir / "xcresult-attachment-export.json",
        {
            "selection": (
                "first failed case; at most one named teardown AX "
                "and one screenshot; no recordings"
            ),
            "attachments": receipt,
        },
    )


def finish_xcode_run(
    result: CommandResult,
    *,
    result_bundle: Path,
    logs_dir: Path,
    expected_target: str,
    log: Callable[[str], None],
    expected_cases: tuple[str, ...] | None = None,
) -> dict[str, int]:
    primary: UIE2EError | None = None
    if result.timed_out:
        primary = UIE2EError(
            f"xcodebuild test exceeded its {COMMAND_TIMEOUT_SECONDS // 60}-minute deadline."
        )
    elif result.output_limited:
        primary = UIE2EError(
            f"xcodebuild test exceeded its {MAX_COMMAND_LOG_BYTES // (1024 * 1024)} MiB log limit."
        )
    elif result.returncode != 0:
        primary = UIE2EError(f"xcodebuild test exited {result.returncode}.")
    if not result_bundle.is_dir():
        log("xcresult diagnostics unavailable: no result bundle was created.")
        raise primary or UIE2EError("xcodebuild exited 0 without creating an xcresult bundle.")

    outputs: dict[str, object] = {}
    errors: list[Exception] = []
    deadline = time.monotonic() + RESULT_DIAGNOSTIC_TIMEOUT_SECONDS

    def extract(kind: str, command: list[str], *, optional: bool = False) -> None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            error = UIE2EError("shared 240-second xcresult diagnostic deadline exhausted")
            errors.append(error)
            log(f"xcresult {kind} omitted: {error}")
            return
        path = logs_dir / f"xcresult-{kind}.json"
        try:
            captured = run_process(
                command,
                log_path=path,
                timeout_seconds=min(30 if optional else 120, remaining),
                max_output_bytes=512 * 1024 if optional else MAX_RESULT_SUMMARY_BYTES,
                cwd=ROOT,
            )
            _require_result(captured, f"xcresult {kind}", path)
            outputs[kind] = _read_json_file(path)
        except Exception as error:
            errors.append(error)
            log(f"xcresult {kind} extraction failed: {type(error).__name__}: {str(error)[:512]}")

    for kind in ("summary", "tests"):
        extract(kind, build_xcresulttool_command(kind, result_bundle))

    failed_cases = [
        node
        for node in _all_dicts(outputs.get("tests"))
        if str(node.get("nodeType", "")).lower() == "test case"
        and str(node.get("result", "")).lower() == "failed"
    ]
    if failed_cases:
        case = failed_cases[0]
        log(f"failed UI case: {str(case.get('name', 'unknown'))[:512]}")
        test_id = case.get("nodeIdentifierURL") or case.get("nodeIdentifier")
        if isinstance(test_id, str) and 0 < len(test_id) <= 2048:
            for kind, filename in (
                ("test-details", "failure-details"),
                ("activities", "failure-activities"),
            ):
                extract(
                    filename,
                    ["xcrun", "xcresulttool", "get", "test-results", kind,
                     "--path", str(result_bundle), "--test-id", test_id],
                    optional=True,
                )
            try:
                export_failure_attachments(
                    outputs.get("failure-activities"),
                    result_bundle=result_bundle,
                    logs_dir=logs_dir,
                    deadline=deadline,
                    log=log,
                )
            except Exception as error:
                log(
                    f"xcresult attachment receipt failed: "
                    f"{type(error).__name__}: {str(error)[:512]}"
                )
        else:
            log("Failure details/attachments omitted: no bounded test identifier in xcresult.")

    reasons: list[str] = []
    for payload in outputs.values():
        for node in _all_dicts(payload):
            candidates = [node.get(key) for key in ("message", "failureText", "issueDescription")]
            if str(node.get("nodeType", "")).lower() == "failure message":
                candidates.append(node.get("name"))
            if node.get("isAssociatedWithFailure") is True:
                candidates.append(node.get("title"))
            for candidate in candidates:
                if isinstance(candidate, str) and candidate.strip():
                    reason = " ".join(candidate.split())[:512]
                    if reason not in reasons and len(reasons) < 5:
                        reasons.append(reason)
    for reason in reasons:
        log(f"xcresult diagnostic: {reason}")
    if primary is not None:
        if not reasons:
            log("xcresult supplied no diagnostic message; inspect retained summary/tests and extraction diagnostics.")
        raise primary
    if errors:
        raise UIE2EError(f"Required xcresult diagnostics failed: {str(errors[0])[:512]}") from errors[0]
    counts = validate_xcresult(
        outputs.get("summary"), outputs.get("tests"), expected_target, expected_cases
    )
    _write_json(logs_dir / "result-validation.json", counts)
    return counts


class ArtifactStager:
    """Copy only an explicit small allowlist, never fixture or build directories."""

    ALLOWED_FILES = {
        "runner.log",
        "server.log",
        "task-fixture-build.log",
        "task-fixture-bin-path.log",
        "task-fixture.log",
        "task-fixture-ready.json",
        "task-fixture-metrics.json",
        "task-fixture-validation.json",
        "xcodebuild.log",
        "xcresult-summary.json",
        "xcresult-tests.json",
        "xcresult-failure-details.json",
        "xcresult-failure-activities.json",
        "xcresult-attachment-export.json",
        "xcresult-failure-ax.txt",
        "xcresult-failure-screenshot.png",
        "xcresult-failure-ax.txt.export.log",
        "xcresult-failure-screenshot.png.export.log",
        "result-validation.json",
        "artifact-staging.log",
    }

    def __init__(self, destination: Path, max_bytes: int = MAX_ARTIFACT_BYTES) -> None:
        self.destination = destination
        self.max_bytes = max_bytes
        self.used_bytes = 0
        self.report_bytes = min(MAX_ARTIFACT_REPORT_BYTES, max_bytes // 4)
        self.diagnostics: list[str] = []
        self.destination.mkdir(parents=True, exist_ok=True, mode=0o700)
        if any(self.destination.iterdir()):
            raise UIE2EError(
                f"Artifact directory must be empty before use: {self.destination}"
            )

    def note(self, message: str) -> None:
        message = " ".join(message.split())[:512]
        if len(self.diagnostics) < 12:
            self.diagnostics.append(message)
        print(f"[ui-e2e] artifact: {message}", flush=True)

    def stage_file(self, source: Path) -> None:
        if source.name not in self.ALLOWED_FILES:
            raise UIE2EError(f"Artifact file is not allowlisted: {source.name}")
        if not source.is_file() or source.is_symlink():
            return
        size = source.stat().st_size
        limit = self.max_bytes if source.name == "artifact-staging.log" else self.max_bytes - self.report_bytes
        file_limit = FAILURE_ATTACHMENT_LIMITS.get(source.name, MAX_COMMAND_LOG_BYTES)
        if source.name.startswith("task-fixture-") and source.name.endswith(".json"):
            file_limit = MAX_FIXTURE_JSON_BYTES
        elif source.name == "task-fixture.log":
            file_limit = MAX_TASK_FIXTURE_LOG_BYTES
        if source.name.endswith(".export.log"):
            file_limit = MAX_ATTACHMENT_EXPORT_LOG_BYTES
        elif source.name == "xcresult-attachment-export.json":
            file_limit = MAX_ARTIFACT_REPORT_BYTES
        if size > file_limit or self.used_bytes + size > limit:
            self.note(f"{source.name} omitted: file or aggregate byte cap exceeded ({size} bytes).")
            return
        shutil.copyfile(source, self.destination / source.name)
        self.used_bytes += size
        if source.name in FAILURE_ATTACHMENT_LIMITS:
            self.note(
                f"{source.name} staged: {size} bytes "
                f"(aggregate used={self.used_bytes}/{self.max_bytes})."
            )

    def stage_result_bundle(self, source: Path) -> bool:
        if not source.is_dir() or source.is_symlink():
            self.note("result.xcresult omitted: source bundle is missing or is not a regular directory.")
            return False
        staged = self.destination / "result.xcresult"
        temp = self.destination / f".result-{uuid.uuid4().hex}.tmp"
        count = 0
        entries = 0
        deadline = time.monotonic() + 30
        try:
            temp.mkdir(mode=0o700)
            for current, directories, files in os.walk(source, followlinks=False):
                entries += len(directories) + len(files)
                if entries > 4096 or time.monotonic() > deadline:
                    self.note("result.xcresult omitted: 4096-entry/30-second staging budget exceeded.")
                    shutil.rmtree(temp)
                    return False
                current_path = Path(current)
                safe_dirs: list[str] = []
                for name in directories:
                    entry = current_path / name
                    if entry.is_symlink():
                        raise UIE2EError("xcresult contains an unexpected symlink.")
                    safe_dirs.append(name)
                    (temp / entry.relative_to(source)).mkdir(
                        parents=True, exist_ok=True, mode=0o700
                    )
                directories[:] = safe_dirs
                for name in files:
                    entry = current_path / name
                    if entry.is_symlink() or not entry.is_file():
                        raise UIE2EError("xcresult contains a non-regular file.")
                    target = temp / entry.relative_to(source)
                    target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                    with entry.open("rb") as reader, target.open("xb") as writer:
                        while chunk := reader.read(64 * 1024):
                            count += len(chunk)
                            if (
                                count > MAX_RESULT_BUNDLE_BYTES
                                or self.used_bytes + count > self.max_bytes - self.report_bytes
                                or time.monotonic() > deadline
                            ):
                                self.note(
                                    "result.xcresult omitted: bundle/aggregate byte or 30-second time cap "
                                    f"exceeded (bundle cap={MAX_RESULT_BUNDLE_BYTES}, artifact cap={self.max_bytes}, "
                                    f"observed={count} bytes); diagnostic texts retained."
                                )
                                shutil.rmtree(temp)
                                return False
                            writer.write(chunk)
            temp.rename(staged)
            self.used_bytes += count
            return True
        except Exception as error:
            self.note(f"result.xcresult staging failed: {type(error).__name__}: {str(error)[:384]}")
            if temp.exists():
                shutil.rmtree(temp)
            raise


def _private_work_dir() -> Path:
    base = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir())).resolve()
    base.mkdir(parents=True, exist_ok=True)
    work_dir = Path(tempfile.mkdtemp(prefix="tvos-net-player-ui-e2e-", dir=base))
    work_dir.chmod(0o700)
    return work_dir


def _artifact_dir(platform: str) -> Path:
    configured = os.environ.get("UI_E2E_ARTIFACT_DIR")
    if configured:
        return Path(configured).expanduser().resolve()
    return Path(tempfile.gettempdir()) / "tvos-net-player-ui-e2e-artifacts" / (
        f"{platform}-{uuid.uuid4().hex}"
    )


def _stage_artifacts(
    stager: ArtifactStager,
    *,
    work_dir: Path,
    result_bundle: Path,
) -> None:
    try:
        for source_name in (
            "runner.log",
            "xcresult-summary.json",
            "xcresult-tests.json",
            "xcresult-failure-details.json",
            "xcresult-failure-activities.json",
            "xcresult-attachment-export.json",
            "xcresult-failure-ax.txt",
            "xcresult-failure-screenshot.png",
            "xcresult-failure-ax.txt.export.log",
            "xcresult-failure-screenshot.png.export.log",
            "result-validation.json",
            "server.log",
            "task-fixture-build.log",
            "task-fixture-bin-path.log",
            "task-fixture.log",
            "task-fixture-ready.json",
            "task-fixture-metrics.json",
            "task-fixture-validation.json",
            "xcodebuild.log",
        ):
            source = work_dir / "logs" / source_name
            if source.exists():
                stager.stage_file(source)
        stager.stage_result_bundle(result_bundle)
    except Exception as error:
        stager.note(f"Artifact staging error: {type(error).__name__}: {str(error)[:384]}")
        raise
    finally:
        report = work_dir / "logs" / "artifact-staging.log"
        text = "\n".join(stager.diagnostics) or "Existing allowlisted artifacts staged within byte budgets."
        report.write_bytes((text + "\n").encode("utf-8")[:stager.report_bytes])
        stager.stage_file(report)


def defer_workspace_cleanup(
    cleanup: CleanupStack,
    stager: ArtifactStager,
    *,
    work_dir: Path,
    result_bundle: Path,
) -> None:
    """Arrange teardown so logs/results are staged before private files vanish."""
    cleanup.defer(
        "private fixture and result directory",
        lambda: shutil.rmtree(work_dir) if work_dir.exists() else None,
    )
    cleanup.defer(
        "allowlisted artifact staging",
        lambda: _stage_artifacts(
            stager, work_dir=work_dir, result_bundle=result_bundle
        ),
    )


def _simulator_destination(udid: str) -> str:
    return f"platform=tvOS Simulator,id={udid}"


def run_ui_e2e(platform: str) -> int:
    run_deadline = time.monotonic() + TOTAL_RUN_BUDGET_SECONDS
    if platform not in PLATFORMS:
        raise UIE2EError(f"Unsupported UI E2E platform: {platform}")
    if sys.platform != "darwin":
        raise UIE2EError("UI E2E runner requires macOS.")

    config = PLATFORMS[platform]
    artifact_dir = _artifact_dir(platform)
    stager = ArtifactStager(artifact_dir)
    work_dir = _private_work_dir()
    logs_dir = work_dir / "logs"
    logs_dir.mkdir(mode=0o700)
    runner_log = logs_dir / "runner.log"
    result_bundle = work_dir / "UIE2E.xcresult"
    def log(message: str) -> None:
        line = f"[ui-e2e:{platform}] {message}"
        print(line, flush=True)
        with runner_log.open("a", encoding="utf-8") as output:
            output.write(line + "\n")

    cleanup = CleanupStack(
        on_failure=lambda error: log(f"failed: {type(error).__name__}: {error}")
    )
    defer_workspace_cleanup(
        cleanup,
        stager,
        work_dir=work_dir,
        result_bundle=result_bundle,
    )

    with cleanup:
            mute_system_audio(logs_dir)
            log("macOS system output mute is set and verified.")
            _record_tool_inventory(platform, logs_dir, runner_log)

            scheme_path = (
                ROOT
                / "TVOSNetPlayer.xcodeproj"
                / "xcshareddata"
                / "xcschemes"
                / f"{config.scheme}.xcscheme"
            )
            verify_scheme_contract(scheme_path, config.test_target)
            log(f"validated shared scheme and endpoint mapping: {config.scheme}")

            build_seconds = min(
                FIXTURE_BUILD_TIMEOUT_SECONDS,
                run_deadline - time.monotonic() - SERVER_READY_TIMEOUT_SECONDS,
            )
            if build_seconds <= 0:
                raise UIE2EError("UI E2E total runtime budget expired before fixture build.")
            task_fixture_binary = build_task_fixture(logs_dir, timeout_seconds=build_seconds)
            log(f"built SwiftPM debug product {TASK_FIXTURE_PRODUCT}.")

            server_binary = resolve_server_binary()
            log(f"using existing cache server binary: {server_binary.name}")
            cache_dir = create_fixture_media(work_dir, logs_dir)
            state_dir = work_dir / "server-state"
            state_dir.mkdir(mode=0o700)
            grpc_port, media_port = allocate_loopback_ports()
            endpoint = validate_server_address(f"http://127.0.0.1:{grpc_port}")
            server = BoundedOwnedProcess(
                build_server_argv(
                    server_binary,
                    cache_dir=cache_dir,
                    state_dir=state_dir,
                    grpc_port=grpc_port,
                    media_port=media_port,
                ),
                log_path=logs_dir / "server.log",
                max_output_bytes=MAX_SERVER_LOG_BYTES,
            )
            cleanup.defer("owned Rust cache server", server.stop)
            wait_for_server(
                server, grpc_port=grpc_port, media_port=media_port
            )
            log(
                f"owned cache server is ready on loopback; 55 fixtures at {cache_dir.name}."
            )

            media_url = build_rust_fixture_media_url(media_port)
            verify_media_fixture_url(
                media_url,
                (cache_dir / "Fixture 055.mp4").stat().st_size,
            )
            ready_file = logs_dir / "task-fixture-ready.json"
            metrics_file = logs_dir / "task-fixture-metrics.json"
            task_fixture = BoundedOwnedProcess(
                build_task_fixture_command(
                    task_fixture_binary,
                    ready_file=ready_file,
                    metrics_file=metrics_file,
                    media_url=media_url,
                ),
                log_path=logs_dir / "task-fixture.log",
                max_output_bytes=MAX_TASK_FIXTURE_LOG_BYTES,
            )
            task_fixture_stopped = False

            def stop_task_fixture() -> None:
                nonlocal task_fixture_stopped
                if not task_fixture_stopped:
                    task_fixture.stop()
                    task_fixture_stopped = True

            cleanup.defer("owned Swift task fixture", stop_task_fixture)
            task_endpoint = wait_for_task_fixture(task_fixture, ready_file)
            log("owned Swift task fixture is ready on loopback.")

            if platform == "tvos":
                simulator_udid = create_owned_tvos_simulator(
                    logs_dir=logs_dir, cleanup=cleanup
                )
                destination = _simulator_destination(simulator_udid)
                log("created and booted one private tvOS simulator from installed runtime.")
            else:
                destination = "platform=macOS"

            derived_override = os.environ.get("DERIVED_DATA_PATH")
            derived_data_path = (
                Path(derived_override).expanduser().resolve()
                if derived_override
                else work_dir / "DerivedData"
            )
            derived_data_path.mkdir(parents=True, exist_ok=True)
            command = build_xcodebuild_command(
                platform=platform,
                server_address=endpoint,
                task_server_address=task_endpoint,
                app_bundle_identifier=(
                    f"dev.joey.tvos-net-player.uie2e.{uuid.uuid4().hex}"
                ),
                derived_data_path=derived_data_path,
                result_bundle_path=result_bundle,
                destination=destination,
            )
            child_env = os.environ.copy()
            child_env["UI_E2E_SERVER_ADDRESS"] = endpoint
            child_env["UI_E2E_TASK_SERVER_ADDRESS"] = task_endpoint
            xcode_log = logs_dir / "xcodebuild.log"
            log("starting actual xcodebuild test execution.")
            test_timeout = min(COMMAND_TIMEOUT_SECONDS, run_deadline - time.monotonic())
            if test_timeout <= 0:
                raise UIE2EError("UI E2E total runtime budget expired before xcodebuild test.")
            xcode_result = run_process(
                command,
                log_path=xcode_log,
                timeout_seconds=test_timeout,
                max_output_bytes=MAX_COMMAND_LOG_BYTES,
                env=child_env,
                cwd=ROOT,
            )
            counts = finish_xcode_run(
                xcode_result,
                result_bundle=result_bundle,
                logs_dir=logs_dir,
                expected_target=config.test_target,
                log=log,
                expected_cases=EXPECTED_CASES_BY_PLATFORM[platform],
            )
            if not task_fixture.is_running():
                raise UIE2EError("Owned task fixture exited before UI test completion.")
            stop_task_fixture()
            metrics = validate_task_fixture_metrics(
                _read_bounded_json(metrics_file, description="Task fixture metrics")
            )
            _write_json(logs_dir / "task-fixture-validation.json", metrics)
            log("task fixture metrics prove one task creation and successful watch/output coverage per scenario.")
            log(
                "xcresult proves target execution: "
                f"{counts['target_passed']} passed, "
                f"{counts['target_skipped']} skipped, "
                f"{counts['target_failed']} failed."
            )

            log("xcresult validation passed; stopping owned resources and staging artifacts.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=sorted(PLATFORMS), required=True)
    args = parser.parse_args(argv)
    if os.name != "posix":
        parser.error("UI E2E runner requires POSIX process-group support.")

    def interrupt_handler(signum: int, _frame: object) -> None:
        raise KeyboardInterrupt(f"received signal {signum}")

    signal.signal(signal.SIGTERM, interrupt_handler)
    signal.signal(signal.SIGINT, interrupt_handler)
    try:
        return run_ui_e2e(args.platform)
    except KeyboardInterrupt as error:
        print(f"[ui-e2e] interrupted: {error}", file=sys.stderr)
        return 130
    except UIE2EError as error:
        print(f"[ui-e2e] error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
