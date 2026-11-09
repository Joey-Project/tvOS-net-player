#!/usr/bin/env python3
"""Run bounded, credential-free native media acceptance tests on macOS."""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import os
import re
import selectors
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent
UI_SCRIPT = Path(__file__).with_name("ui-e2e.py")
TOTAL_BUDGET_SECONDS = 50 * 60
COMPILE_BUDGET_SECONDS = 25 * 60
RUNTIME_BUDGET_SECONDS = 24 * 60
FINALIZE_BUDGET_SECONDS = 60
COMMAND_LOG_BYTES = 8 * 1024 * 1024
ARTIFACT_BYTES = 32 * 1024 * 1024
MIN_ARTIFACT_FREE_BYTES = 6 * 1024 * 1024 * 1024
REPORT_BYTES = 32 * 1024
EXPECTED_TESTS = (
    "media_acceptance_tests::native_progressive_adaptation_and_recovery",
    "media_acceptance_tests::native_transcoded_output_decodes",
    "media_acceptance_tests::native_progressive_buffered_fallback_keeps_playing",
)
EXTENDED_DIAGNOSTIC_TEST = (
    "media_acceptance_tests::native_diagnostic_extended_adaptation_and_recovery"
)
EXPECTED_CASES = {
    EXPECTED_TESTS[2]: {
        "report": "buffered.json", "scenario": "buffered_continuity", "duration": 160,
        "probe_duration": 130, "probe_deadline": 175, "sizes": ((640, 360), (320, 180)),
    },
    EXPECTED_TESTS[0]: {
        "report": "abr.json", "scenario": "unbuffered_recovery", "duration": 3600,
        "probe_duration": 600, "probe_deadline": 720, "diagnostic_only": False,
        "sizes": ((640, 360), (320, 180), (640, 360)),
    },
    EXPECTED_TESTS[1]: {
        "report": "transcoding.json", "scenario": "transcoded_output", "duration": 45,
        "probe_duration": 30, "probe_deadline": 45, "sizes": ((640, 360),),
    },
}
EXPECTED_ABR_SIZES = EXPECTED_CASES[EXPECTED_TESTS[0]]["sizes"]
EXPECTED_TRANSCODE_SIZES = EXPECTED_CASES[EXPECTED_TESTS[1]]["sizes"]
SAFE_ENV_KEYS = (
    "PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
    "TERM", "DEVELOPER_DIR", "SDKROOT", "TOOLCHAINS", "SWIFT_EXEC",
    "SWIFT_DRIVER_SWIFT_FRONTEND_EXEC", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR",
)


class MediaNetworkError(RuntimeError):
    pass


def _load_ui_helpers() -> Any:
    spec = importlib.util.spec_from_file_location("media_network_ui_e2e", UI_SCRIPT)
    if spec is None or spec.loader is None:
        raise MediaNetworkError("Could not load the existing UI E2E process helpers.")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


ui = _load_ui_helpers()


def clean_environment(source: dict[str, str] | None = None) -> dict[str, str]:
    source = os.environ if source is None else source
    result = {key: source[key] for key in SAFE_ENV_KEYS if key in source}
    result.setdefault("PATH", os.defpath)
    return result


def build_compile_argv(binary: Path) -> list[str]:
    return [
        "xcrun", "swiftc", "-swift-version", "6", "-strict-concurrency=complete",
        "-warnings-as-errors", "-parse-as-library", str(ROOT / "scripts" / "probe-lan-playback.swift"),
        "-o", str(binary),
    ]


def build_cargo_argv() -> list[str]:
    return [
        "cargo", "test", "--manifest-path", str(ROOT / "Cargo.toml"), "--package",
        "tvos-net-player-cache-server", "--release", "--locked", "--lib",
        "media_acceptance_tests::", "--", "--ignored", "--nocapture", "--test-threads=1",
        "--skip", EXTENDED_DIAGNOSTIC_TEST,
        "--color=never",
    ]


def build_cargo_compile_argv() -> list[str]:
    return [
        "cargo", "test", "--manifest-path", str(ROOT / "Cargo.toml"), "--package",
        "tvos-net-player-cache-server", "--release", "--locked", "--lib", "--no-run",
        "--color=never",
    ]


def build_signed_test_argv(binary: Path) -> list[str]:
    return [
        str(binary), "media_acceptance_tests::", "--ignored", "--nocapture",
        "--test-threads=1", "--skip", EXTENDED_DIAGNOSTIC_TEST, "--color=never",
    ]


def _require_tools(env: dict[str, str]) -> None:
    missing = [name for name in ("ffmpeg", "ffprobe", "xcrun", "swiftc", "cargo") if shutil.which(name, path=env["PATH"]) is None]
    if sys.platform != "darwin":
        missing.append("macOS")
    if missing:
        raise MediaNetworkError("Required native media acceptance tools unavailable: " + ", ".join(missing))


def _new_private_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.chmod(0o700)


def _fresh_artifact_dir(configured: str | None) -> Path:
    requested = (Path(configured).expanduser() if configured else
                 ROOT / "build" / "MediaNetworkE2E" / "artifacts" / uuid.uuid4().hex)
    if requested.is_symlink():
        raise MediaNetworkError("Artifact directory must not be a symlink.")
    path = requested.resolve()
    if path.exists() and (not path.is_dir() or any(path.iterdir())):
        raise MediaNetworkError(f"Artifact directory must be an empty, non-symlink directory: {path}")
    _new_private_dir(path)
    return path


def _artifact_size(directory: Path, *, limit: int = ARTIFACT_BYTES) -> int:
    total = 0
    entries = 0
    for current, dirs, files in os.walk(directory, followlinks=False):
        base = Path(current)
        entries += len(dirs) + len(files)
        if entries > 512:
            raise MediaNetworkError("Artifact tree exceeded its 512-entry bound.")
        for name in dirs + files:
            item = base / name
            if item.is_symlink():
                raise MediaNetworkError(f"Artifact tree contains an unexpected symlink: {item.name}")
        for name in files:
            item = base / name
            if not item.is_file():
                raise MediaNetworkError(f"Artifact tree contains a non-regular file: {item.name}")
            total += item.stat().st_size
            if total > limit:
                raise MediaNetworkError(f"Artifact tree exceeded the {limit}-byte aggregate cap.")
    return total


def _require_artifact_free_space(directory: Path) -> None:
    if shutil.disk_usage(directory).free < MIN_ARTIFACT_FREE_BYTES:
        raise MediaNetworkError("Artifact volume has insufficient free space for native media acceptance.")


def resolve_work_deadline(configured: str | None, *, started_at: float, now: float | None = None) -> float:
    now = time.monotonic() if now is None else now
    deadline = started_at + TOTAL_BUDGET_SECONDS
    if configured is not None:
        try:
            external_deadline = float(configured)
        except (TypeError, ValueError, OverflowError):
            raise MediaNetworkError("Workflow work deadline is invalid.") from None
        if not math.isfinite(external_deadline):
            raise MediaNetworkError("Workflow work deadline is invalid.")
        deadline = min(deadline, external_deadline)
    if deadline <= now:
        raise MediaNetworkError("Workflow or runner work deadline has expired.")
    return deadline


def remaining_work(deadline: float, *, now: float | None = None) -> float:
    remaining = deadline - (time.monotonic() if now is None else now)
    if remaining <= 0:
        raise MediaNetworkError("Workflow or runner work deadline has expired.")
    return remaining


def resolve_compile_deadline(*, started_at: float, total_deadline: float) -> float:
    return min(
        started_at + COMPILE_BUDGET_SECONDS,
        total_deadline - RUNTIME_BUDGET_SECONDS - FINALIZE_BUDGET_SECONDS,
    )


def _write_summary(artifact_dir: Path, *, status: str, message: str, started: float) -> None:
    summary = {
        "schema_version": 1,
        "status": status,
        "message": " ".join(message.split())[:1024],
        "elapsed_seconds": round(time.monotonic() - started, 3),
    }
    encoded = (json.dumps(summary, sort_keys=True, indent=2) + "\n").encode("utf-8")
    if len(encoded) > 4096 or _artifact_size(artifact_dir) + len(encoded) > ARTIFACT_BYTES:
        raise MediaNetworkError("Could not retain the bounded runner summary within artifact limits.")
    (artifact_dir / "summary.json").write_bytes(encoded)


def _run_command(
    argv: list[str], *, log_path: Path, env: dict[str, str], timeout: float,
    artifact_dir: Path, cargo_artifact_monitor: bool = False,
) -> ui.CommandResult:
    if not cargo_artifact_monitor:
        available = ARTIFACT_BYTES - _artifact_size(artifact_dir)
        limit = min(COMMAND_LOG_BYTES, available)
        if limit <= 0:
            raise MediaNetworkError("No artifact budget remains for command output.")
        result = ui.run_process(
            argv, log_path=log_path, timeout_seconds=timeout,
            max_output_bytes=limit, env=env, cwd=ROOT,
        )
        if result.timed_out:
            raise MediaNetworkError(f"Command timed out; bounded log: {log_path.name}")
        if result.output_limited:
            raise MediaNetworkError(f"Command exceeded its output limit; bounded log: {log_path.name}")
        if result.returncode != 0:
            raise MediaNetworkError(f"Command exited {result.returncode}; bounded log: {log_path.name}")
        return result

    if timeout <= 0:
        raise MediaNetworkError("No runtime budget remains for the native tests.")
    log_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    deadline = time.monotonic() + timeout
    process: subprocess.Popen[bytes] | None = None
    written = 0
    timed_out = False
    limited = False
    selector = selectors.DefaultSelector()
    try:
        with log_path.open("xb", buffering=0) as log:
            process = subprocess.Popen(
                argv, cwd=ROOT, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT, start_new_session=True, close_fds=True,
            )
            assert process.stdout is not None
            os.set_blocking(process.stdout.fileno(), False)
            selector.register(process.stdout, selectors.EVENT_READ)
            while process.poll() is None or selector.get_map():
                if time.monotonic() >= deadline:
                    timed_out = True
                    break
                if _artifact_size(artifact_dir) > ARTIFACT_BYTES:
                    limited = True
                    break
                for key, _ in selector.select(0.2):
                    chunk = os.read(key.fd, 64 * 1024)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    available = min(COMMAND_LOG_BYTES - written,
                                    ARTIFACT_BYTES - _artifact_size(artifact_dir) - len(chunk))
                    if available < len(chunk):
                        if available > 0:
                            saved = log.write(chunk[:available])
                            if saved != available:
                                raise OSError("short write while retaining bounded command output")
                            written += saved
                        limited = True
                        break
                    saved = log.write(chunk)
                    if saved != len(chunk):
                        raise OSError("short write while retaining bounded command output")
                    written += saved
                if limited:
                    break
            selector.close()
            ui.stop_process_group(process)
            if process.stdout is not None:
                process.stdout.close()
            log.flush()
    except MediaNetworkError:
        selector.close()
        if process is not None:
            ui.stop_process_group(process)
        raise
    except (OSError, subprocess.SubprocessError) as error:
        selector.close()
        if process is not None:
            ui.stop_process_group(process)
        raise MediaNetworkError(f"Could not supervise native test process: {type(error).__name__}") from error
    finally:
        selector.close()
        if process is not None and process.stdout is not None:
            process.stdout.close()
    assert process is not None
    result = ui.CommandResult(process.wait(timeout=0), written, timed_out, limited)
    if result.timed_out:
        raise MediaNetworkError(f"Native tests exceeded their {timeout:g}-second deadline; log: {log_path.name}")
    if result.output_limited:
        raise MediaNetworkError(f"Native tests exceeded the artifact/log byte cap; log: {log_path.name}")
    if result.returncode != 0:
        raise MediaNetworkError(f"Native tests exited {result.returncode}; bounded log: {log_path.name}")
    return result


def _mute_system_audio(logs: Path, env: dict[str, str]) -> None:
    set_log = logs / "system-mute-set.log"
    result = ui.run_process(
        ["osascript", "-e", "set volume output muted true"], log_path=set_log,
        timeout_seconds=10, max_output_bytes=4096, env=env,
    )
    ui._require_result(result, "system-mute-set", set_log)
    verify_log = logs / "system-mute-verify.log"
    result = ui.run_process(
        ["osascript", "-e", "output muted of (get volume settings)"], log_path=verify_log,
        timeout_seconds=10, max_output_bytes=4096, env=env,
    )
    ui._require_result(result, "system-mute-verify", verify_log)
    if verify_log.read_text(encoding="utf-8", errors="replace").strip().lower() != "true":
        raise MediaNetworkError("Could not verify macOS system output is muted.")


def validate_cargo_log(path: Path, *, returncode: int = 0, timed_out: bool = False) -> None:
    if timed_out:
        raise MediaNetworkError("Cargo test process timed out.")
    if returncode != 0:
        raise MediaNetworkError(f"Cargo test process exited {returncode}.")
    try:
        with path.open("rb") as log:
            raw = log.read(COMMAND_LOG_BYTES + 1)
        if len(raw) > COMMAND_LOG_BYTES:
            raise MediaNetworkError("Cargo test log exceeds its output bound.")
        text = raw.decode("utf-8")
    except MediaNetworkError:
        raise
    except (OSError, UnicodeError) as error:
        raise MediaNetworkError("Cargo test log is missing or invalid.") from error
    expected = set(EXPECTED_TESTS)
    seen: set[str] = set()
    pending: str | None = None
    summaries: list[int] = []
    summary_pattern = re.compile(
        r"test result: ok\. 3 passed; 0 failed; 0 ignored; 0 measured; "
        r"\d+ filtered out(?:; finished in \d+(?:\.\d+)?s)?"
    )
    for index, line in enumerate(text.splitlines()):
        if line.startswith("test result:"):
            if pending is not None or summary_pattern.fullmatch(line) is None:
                raise MediaNetworkError("Cargo log contains a malformed or premature test summary.")
            summaries.append(index)
            continue
        if line.startswith("test "):
            if pending is not None:
                raise MediaNetworkError("Cargo log contains an unterminated Rust test record.")
            match = re.fullmatch(r"test (.+) \.\.\.(?: (.*))?", line)
            if match is None:
                raise MediaNetworkError("Cargo log contains a malformed Rust test header.")
            name, status = match.groups()
            if name not in expected:
                raise MediaNetworkError(f"Cargo log contains an unexpected Rust test: {name}.")
            if name in seen:
                raise MediaNetworkError(f"Cargo log contains a duplicate Rust test: {name}.")
            seen.add(name)
            if status is None:
                pending = name
            elif status == "ok":
                pass
            elif status in {"FAILED", "ignored", "bench"}:
                raise MediaNetworkError(f"Cargo test {name} did not pass: {status}.")
            elif status.startswith(("ok ", "FAILED ", "ignored ", "bench ")):
                raise MediaNetworkError(f"Cargo test {name} has an ambiguous inline status.")
            else:
                pending = name
            continue
        if line in {"ok", "FAILED", "ignored", "bench"}:
            if line != "ok" or pending is None:
                raise MediaNetworkError("Cargo log contains an orphan or non-passing test status.")
            pending = None
    if pending is not None:
        raise MediaNetworkError(f"Cargo test {pending} has no passing completion status.")
    if seen != expected:
        raise MediaNetworkError("Cargo log did not contain exactly the three required Rust tests.")
    if len(summaries) != 1:
        raise MediaNetworkError("Cargo log must contain exactly one complete test result summary.")
    if any(line.strip() for line in text.splitlines()[summaries[0] + 1:]):
        raise MediaNetworkError("Cargo log contains data after its terminal test summary.")


def _read_report(path: Path) -> dict[str, Any]:
    if path.is_symlink() or not path.is_file() or path.stat().st_size > REPORT_BYTES:
        raise MediaNetworkError(f"Required report is missing, non-regular, or oversized: {path.name}")
    try:
        value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=_unique_object)
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
        raise MediaNetworkError(f"Report JSON is truncated or invalid: {path.name}") from error
    if not isinstance(value, dict):
        raise MediaNetworkError(f"Report must be a JSON object: {path.name}")
    return value


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("duplicate JSON key")
        value[key] = item
    return value


def _numeric(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def _exact_int(value: Any, expected: int | None = None) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and (expected is None or value == expected)


def _positive_u64(value: Any) -> bool:
    return type(value) is int and 0 < value <= 2**64 - 1


def _observation_state(
    value: Any, *, expected_at_or_after: float, asset_duration: float,
    must_reach_asset_end: bool, latest_offset: float | None = 5,
) -> tuple[float, float]:
    if not isinstance(value, dict) or not _numeric(value.get("observed_monotonic_seconds")):
        raise MediaNetworkError("Probe report lacks a finite passive observation timestamp.")
    observed = float(value["observed_monotonic_seconds"])
    if observed < expected_at_or_after or (
        latest_offset is not None and observed > expected_at_or_after + latest_offset
    ):
        raise MediaNetworkError("Passive observation is outside the controller recovery window.")
    state = value.get("player_state")
    if not isinstance(state, dict):
        raise MediaNetworkError("Passive observation has no player state.")
    playhead = state.get("playhead_seconds")
    preference = state.get("preferred_forward_buffer_duration_seconds")
    if (not _numeric(playhead) or float(playhead) < 0
            or float(playhead) > asset_duration + 0.25
            or state.get("automatically_waits_to_minimize_stalling") is not True
            or not _numeric(preference) or float(preference) != 0):
        raise MediaNetworkError("Passive observation has a playhead outside source duration or non-default buffering preference.")
    ranges = state.get("loaded_time_ranges")
    if not isinstance(ranges, dict) or ranges.get("truncated") is not False:
        raise MediaNetworkError("Passive observation loaded ranges are missing or truncated.")
    observations = ranges.get("observations")
    source_count, valid_count, invalid_count = (
        ranges.get("source_count"), ranges.get("valid_count"), ranges.get("invalid_count")
    )
    if (not isinstance(observations, list) or not observations or len(observations) > 8
            or not _exact_int(source_count) or not _exact_int(valid_count)
            or not _exact_int(invalid_count, 0) or source_count != valid_count
            or valid_count != len(observations)):
        raise MediaNetworkError("Passive observation loaded ranges are incomplete or invalid.")
    containing_ends: list[float] = []
    for entry in observations:
        if not isinstance(entry, dict):
            raise MediaNetworkError("Passive observation contains a malformed loaded range.")
        start, duration, end = (entry.get("start_seconds"), entry.get("duration_seconds"), entry.get("end_seconds"))
        if (not _numeric(start) or not _numeric(duration) or not _numeric(end)
                or min(float(start), float(duration), float(end)) < 0
                or abs((float(start) + float(duration)) - float(end)) > 0.001
                or float(end) > asset_duration + 0.25):
            raise MediaNetworkError("Passive observation contains an invalid range or one beyond source duration.")
        if float(start) <= float(playhead) <= float(end):
            containing_ends.append(float(end))
    if not containing_ends:
        raise MediaNetworkError("No observed loaded range contains the playhead.")
    range_end = max(containing_ends)
    if must_reach_asset_end and range_end < asset_duration - 0.25:
        raise MediaNetworkError("Steady-play end range does not cover the remaining asset within 250ms.")
    return observed, range_end


def _validate_progressive_origin_trace(report: dict[str, Any], expected_case: str, report_name: str) -> None:
    requests = report.get("requests")
    if not isinstance(requests, dict):
        raise MediaNetworkError(f"Progressive report lacks origin request evidence: {report_name}")
    payload = requests.get("media_payload_by_phase_and_representation")
    counts = requests.get("count_by_phase_and_representation")
    successful = requests.get("successful_by_representation")
    if any(not isinstance(matrix, list) or len(matrix) != 3
           or any(not isinstance(row, list) or len(row) != 3 for row in matrix)
           for matrix in (payload, counts)) or not isinstance(successful, list) or len(successful) != 3:
        raise MediaNetworkError(f"Progressive origin request evidence has invalid dimensions: {report_name}")
    if any(not _exact_int(value) or value < 0 for matrix in (payload, counts) for row in matrix for value in row) \
            or any(not _exact_int(value) or value < 0 for value in successful) \
            or not _exact_int(requests.get("high_fault_status_count")):
        raise MediaNetworkError(f"Progressive origin request evidence contains invalid counts: {report_name}")
    if (payload[0][0] <= 0 or counts[1][0] <= 0 or requests["high_fault_status_count"] <= 0
            or counts[1][1] <= 0 or successful[1] <= 0):
        raise MediaNetworkError(f"Progressive origin trace lacks baseline high, faulted high, or healthy low: {report_name}")
    if expected_case == EXPECTED_TESTS[0] and payload[2][0] <= 0:
        raise MediaNetworkError(f"Future-case origin trace lacks recovered high traffic: {report_name}")


def validate_report(path: Path, expected_case: str, expected_sizes: tuple[tuple[int, int], ...] | None = None) -> dict[str, Any]:
    if expected_case not in EXPECTED_TESTS:
        raise MediaNetworkError("Requested native report case is not in the acceptance contract.")
    case_contract = EXPECTED_CASES[expected_case]
    if expected_sizes is None:
        expected_sizes = case_contract["sizes"]
    report = _read_report(path)
    if not _exact_int(report.get("schema_version"), 1) or report.get("status") != "passed":
        raise MediaNetworkError(f"Report schema/status is not a passed version-1 result: {path.name}")
    if report.get("diagnostic_only") is not False:
        raise MediaNetworkError(f"Report diagnostic_only value is not false for the normal suite: {path.name}")
    case = report.get("case_full_name")
    if case != expected_case:
        raise MediaNetworkError(f"Report case name does not match the requested Rust test: {path.name}")
    if (report.get("scenario") != case_contract["scenario"]
            or not _numeric(report.get("source_duration_seconds"))
            or float(report["source_duration_seconds"]) != case_contract["duration"]):
        raise MediaNetworkError(f"Report scenario or source duration does not match its named case: {path.name}")
    preflight = report.get("hls_playlist_preflight")
    canonical_bandwidth = (
        preflight.get("high_variant_peak_bandwidth_bits_per_second")
        if isinstance(preflight, dict) else None
    )
    advertised_bandwidth = (
        preflight.get("advertised_high_bandwidth_bits_per_second")
        if isinstance(preflight, dict) else None
    )
    if (not isinstance(preflight, dict)
            or preflight.get("bandwidth_semantics") != "rfc8216_contiguous_duration_window"
            or not _positive_u64(canonical_bandwidth)
            or not _positive_u64(advertised_bandwidth)
            or advertised_bandwidth != canonical_bandwidth):
        raise MediaNetworkError(
            f"HLS playlist preflight semantics or exact advertised bandwidth is invalid: {path.name}"
        )
    probe = report.get("probe")
    if not isinstance(probe, dict) or not _exact_int(probe.get("schema_version"), 1) or probe.get("status") != "completed":
        raise MediaNetworkError(f"Probe report is missing its completed version-1 result: {path.name}")
    frames = probe.get("decoded_frame_count")
    if not isinstance(frames, int) or isinstance(frames, bool) or frames <= 0 or probe.get("audio_track_present") is not True:
        raise MediaNetworkError(f"Probe report lacks decoded frames or an audio track: {path.name}")
    expected_duration = case_contract["probe_duration"]
    expected_deadline = case_contract["probe_deadline"]
    if (probe.get("requested_duration_seconds") != expected_duration
            or probe.get("overall_deadline_seconds") != expected_deadline
            or probe.get("deadline_mode") != "cooperative"):
        raise MediaNetworkError(f"Probe duration or cooperative deadline does not match its native case: {path.name}")
    enabled_audio = probe.get("enabled_audio_track_count")
    if not isinstance(enabled_audio, int) or isinstance(enabled_audio, bool) or enabled_audio < 1:
        raise MediaNetworkError(f"Probe report lacks enabled audio-track evidence: {path.name}")
    steps = probe.get("steps")
    required_steps = {"play", "pause", "resume", "seek_forward", "seek_forward_play", "seek_back", "seek_back_play", "rate_1_25", "duration_remainder"}
    if not isinstance(steps, list) or any(
        not any(isinstance(step, dict) and step.get("name") == name and step.get("status") == "completed" for step in steps)
        for name in required_steps
    ):
        raise MediaNetworkError(f"Probe did not prove completion of its playback controls: {path.name}")
    expectations = probe.get("video_size_expectations")
    if not isinstance(expectations, dict) or expectations.get("enabled") is not True or expectations.get("complete") is not True:
        raise MediaNetworkError(f"Probe video-size expectations are not complete: {path.name}")
    if expectations.get("semantics") != "ordered_decoded_size_subsequence":
        raise MediaNetworkError(f"Probe video-size expectation semantics are unexpected: {path.name}")
    if not _exact_int(expectations.get("minimum_distinct_display_timestamps_per_size"), 3):
        raise MediaNetworkError(f"Probe does not require three distinct timestamps per size: {path.name}")
    entries = expectations.get("entries")
    if not isinstance(entries, list) or len(entries) != len(expected_sizes):
        raise MediaNetworkError(f"Probe video-size evidence has an unexpected entry count: {path.name}")
    last_elapsed: float | None = None
    phase_samples: list[list[float]] = []
    for entry, (width, height) in zip(entries, expected_sizes, strict=True):
        if not isinstance(entry, dict) or entry.get("expected_width") != width or entry.get("expected_height") != height:
            raise MediaNetworkError(f"Probe video-size evidence has an unexpected ordered dimension: {path.name}")
        if entry.get("matched") is not True or not _exact_int(entry.get("required_distinct_frame_count"), 3):
            raise MediaNetworkError(f"Probe video-size entry did not satisfy its three-frame requirement: {path.name}")
        count = entry.get("distinct_frame_count")
        if not isinstance(count, int) or isinstance(count, bool) or count < 3:
            raise MediaNetworkError(f"Probe video-size entry has insufficient distinct frames: {path.name}")
        samples = entry.get("samples")
        if not isinstance(samples, list) or len(samples) != 3:
            raise MediaNetworkError(f"Probe video-size entry is missing bounded timing samples: {path.name}")
        timestamps: set[float] = set()
        monotonic_samples: list[float] = []
        for sample in samples:
            if (not isinstance(sample, dict) or not _numeric(sample.get("display_time_seconds"))
                    or not _numeric(sample.get("observed_elapsed_seconds"))
                    or not _numeric(sample.get("observed_monotonic_seconds"))):
                raise MediaNetworkError(f"Probe sample contains nonnumeric or non-finite timing: {path.name}")
            timestamp = float(sample["display_time_seconds"])
            elapsed = float(sample["observed_elapsed_seconds"])
            monotonic = float(sample["observed_monotonic_seconds"])
            if timestamp < 0 or elapsed < 0 or monotonic <= 0:
                raise MediaNetworkError(f"Probe sample contains negative timing: {path.name}")
            if timestamp in timestamps:
                raise MediaNetworkError(f"Probe samples do not have distinct display timestamps: {path.name}")
            if last_elapsed is not None and elapsed < last_elapsed:
                raise MediaNetworkError(f"Probe observed elapsed timing is out of order: {path.name}")
            timestamps.add(timestamp)
            monotonic_samples.append(monotonic)
            last_elapsed = elapsed
        if monotonic_samples != sorted(monotonic_samples):
            raise MediaNetworkError(f"Probe monotonic timestamps are out of order: {path.name}")
        phase_samples.append(monotonic_samples)
    if expected_case != EXPECTED_TESTS[1]:
        _validate_progressive_origin_trace(report, expected_case, path.name)
        timeline = report.get("fault_timeline")
        if not isinstance(timeline, dict) or timeline.get("enabled") is not True or timeline.get("bad_high_status_code") != 503:
            raise MediaNetworkError(f"Progressive report omitted its enabled 503 fault window: {path.name}")
        names = (
            "decoder_ready_monotonic_seconds", "baseline_start_monotonic_seconds",
            "bad_high_before_monotonic_seconds", "bad_high_after_monotonic_seconds",
            "recovery_before_monotonic_seconds", "recovery_after_monotonic_seconds",
        )
        bounds = [timeline.get(name) for name in names]
        if not all(_numeric(value) and value > 0 for value in bounds):
            raise MediaNetworkError(f"Progressive report has incomplete fault-window clock bounds: {path.name}")
        decoder_ready, baseline_start, bad_before, bad_after, recovery_before, recovery_after = map(float, bounds)
        if not (decoder_ready <= baseline_start and baseline_start + 6 <= bad_before
                and bad_before <= bad_after <= recovery_before <= recovery_after):
            raise MediaNetworkError(f"Progressive report fault-window clock bounds are inconsistent: {path.name}")
        if (any(timestamp >= bad_before for timestamp in phase_samples[0])
                or any(timestamp < bad_after or timestamp >= recovery_before for timestamp in phase_samples[1])):
            raise MediaNetworkError(f"Progressive decoded-size samples do not align with the high-low fault phases: {path.name}")
        if expected_case == EXPECTED_TESTS[0] and any(timestamp < recovery_after for timestamp in phase_samples[2]):
            raise MediaNetworkError(f"Future-case decoded-size samples do not prove high recovery: {path.name}")

        recovery_time, recovery_end = _observation_state(
            probe.get("recovery_observation"), expected_at_or_after=recovery_after,
            asset_duration=float(case_contract["duration"]), must_reach_asset_end=False,
        )
        if expected_case == EXPECTED_TESTS[0] and recovery_end > float(case_contract["duration"]) - 20:
            raise MediaNetworkError(f"Future-case observation does not prove 20 seconds remain beyond loaded media: {path.name}")
        if expected_case == EXPECTED_TESTS[2]:
            play_step = next((step for step in steps if isinstance(step, dict) and step.get("name") == "play"), None)
            if not isinstance(play_step, dict) or not _numeric(play_step.get("elapsed_seconds")) \
                    or float(play_step["elapsed_seconds"]) < 110:
                raise MediaNetworkError(f"Buffered case did not retain at least 110 seconds of steady play: {path.name}")
            steady_time, _ = _observation_state(
                probe.get("steady_play_end_observation"), expected_at_or_after=recovery_after,
                asset_duration=float(case_contract["duration"]), must_reach_asset_end=True,
                latest_offset=None,
            )
            if steady_time < recovery_time:
                raise MediaNetworkError(f"Buffered end observation precedes its recovery observation: {path.name}")
    return report


def _load_local_build() -> Any:
    path = ROOT / "scripts" / "local_build.py"
    spec = importlib.util.spec_from_file_location("media_network_local_build", path)
    if spec is None or spec.loader is None:
        raise MediaNetworkError("Could not load the local signing helper.")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _signed_product(receipt: dict[str, Any], target: str) -> Path:
    try:
        product = receipt["products"][target]
        path = Path(product["path"])
        required = ("identifier", "team_id", "certificate_sha1", "designated_requirement", "cdhash")
        if any(not isinstance(product.get(key), str) or not product[key] for key in required):
            raise ValueError
        if not path.is_absolute() or path.is_symlink() or not path.is_file():
            raise ValueError
        return path
    except (AttributeError, KeyError, TypeError, ValueError, OSError) as error:
        raise MediaNetworkError(f"Signing helper did not provide a verified fixed {target} executable.") from error


def _run_local_signed(
    *, logs: Path, artifact_dir: Path, env: dict[str, str], config_file: Path | None,
    total_deadline: float, started: float, rust_artifacts: Path,
) -> None:
    local_build = _load_local_build()
    compile_deadline = resolve_compile_deadline(started_at=started, total_deadline=total_deadline)
    invocation = 0

    def bounded_build_run(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
        nonlocal invocation
        invocation += 1
        log_path = logs / f"signed-compile-{invocation}.log"
        timeout = remaining_work(compile_deadline)
        available = ARTIFACT_BYTES - _artifact_size(artifact_dir)
        limit = min(COMMAND_LOG_BYTES, available)
        if limit <= 0:
            raise local_build.BuildError("No artifact budget remains for signed compiler output.")
        command_env = kwargs.get("env", env)
        if not isinstance(command_env, dict):
            raise local_build.BuildError("signed compiler environment must be a mapping.")
        command_cwd = kwargs.get("cwd", ROOT)
        if not isinstance(command_cwd, (Path, str)):
            raise local_build.BuildError("signed compiler working directory must be a path.")
        result = ui.run_process(
            list(argv), log_path=log_path, timeout_seconds=timeout, max_output_bytes=limit,
            env=clean_environment(command_env), cwd=Path(command_cwd),
        )
        if result.timed_out:
            raise local_build.BuildError(f"signed compiler exceeded its shared deadline; log: {log_path.name}")
        if result.output_limited:
            raise local_build.BuildError(f"signed compiler exceeded its output cap; log: {log_path.name}")
        output = log_path.read_text(encoding="utf-8", errors="replace")
        if result.returncode != 0 and kwargs.get("check", True):
            raise local_build.BuildError(f"signed compiler exited {result.returncode}; log: {log_path.name}")
        return subprocess.CompletedProcess(list(argv), result.returncode, output, "")

    try:
        original_run = local_build.RUN
        local_build.RUN = bounded_build_run
        try:
            with local_build.prepared_targets({"probe", "media-tests"}, config_file=config_file) as receipt:
                probe_binary = _signed_product(receipt, "probe")
                test_binary = _signed_product(receipt, "media-tests")
                _mute_system_audio(logs, env)
                test_env = dict(env)
                test_env["LAN_MEDIA_ACCEPTANCE_PROBE_BINARY"] = str(probe_binary)
                test_env["LAN_MEDIA_ACCEPTANCE_ARTIFACTS"] = str(rust_artifacts)
                timeout = min(
                    RUNTIME_BUDGET_SECONDS,
                    max(0.0, remaining_work(total_deadline) - FINALIZE_BUDGET_SECONDS),
                )
                _run_command(
                    build_signed_test_argv(test_binary), log_path=logs / "cargo-test.log", env=test_env,
                    timeout=timeout, artifact_dir=artifact_dir, cargo_artifact_monitor=True,
                )
                validate_cargo_log(logs / "cargo-test.log")
                for case_name, contract in EXPECTED_CASES.items():
                    validate_report(rust_artifacts / contract["report"], case_name, contract["sizes"])
        finally:
            local_build.RUN = original_run
    except local_build.BuildError as error:
        raise MediaNetworkError(f"Local signed build or verification failed: {error}") from error


def run(
    artifact_dir_arg: str | None = None, *, ci_unsigned: bool = False,
    local_config_arg: str | None = None,
) -> int:
    if ci_unsigned and os.environ.get("GITHUB_ACTIONS", "").lower() != "true":
        raise MediaNetworkError("--ci-unsigned is restricted to GitHub Actions runners.")
    started = time.monotonic()
    work_dir: Path | None = None
    artifact_dir: Path | None = None
    status = "failed"
    message = "Acceptance requirements were not proven."
    exit_code = 1
    try:
        artifact_dir = _fresh_artifact_dir(artifact_dir_arg)
        logs = artifact_dir / "logs"
        rust_artifacts = artifact_dir / "rust"
        _new_private_dir(logs)
        _new_private_dir(rust_artifacts)
        _require_artifact_free_space(artifact_dir)
        env = clean_environment()
        if local_config_arg is not None:
            config_path = Path(local_config_arg).expanduser()
            if not config_path.is_absolute():
                raise MediaNetworkError("--local-config must name an absolute public config path.")
        else:
            config_path = None
        total_deadline = resolve_work_deadline(
            os.environ.get("MEDIA_NETWORK_E2E_JOB_WORK_DEADLINE_MONOTONIC"), started_at=started
        )
        if ci_unsigned:
            _require_tools(env)
            base = Path(env.get("TMPDIR", tempfile.gettempdir())).resolve()
            base.mkdir(parents=True, exist_ok=True)
            work_dir = Path(tempfile.mkdtemp(prefix="media-network-e2e-", dir=base))
            work_dir.chmod(0o700)
            probe_binary = work_dir / "probe-lan-playback"
            compile_deadline = resolve_compile_deadline(started_at=time.monotonic(), total_deadline=total_deadline)
            _run_command(build_compile_argv(probe_binary), log_path=logs / "swift-compile.log",
                         env=env, timeout=remaining_work(compile_deadline), artifact_dir=artifact_dir)
            if not probe_binary.is_file() or probe_binary.is_symlink():
                raise MediaNetworkError("Swift compiler exited successfully without producing the native probe.")
            _run_command(build_cargo_compile_argv(), log_path=logs / "cargo-compile.log", env=env,
                         timeout=remaining_work(compile_deadline), artifact_dir=artifact_dir,
                         cargo_artifact_monitor=True)
            _mute_system_audio(logs, env)
            test_env = dict(env)
            test_env["LAN_MEDIA_ACCEPTANCE_PROBE_BINARY"] = str(probe_binary)
            test_env["LAN_MEDIA_ACCEPTANCE_ARTIFACTS"] = str(rust_artifacts)
            timeout = min(RUNTIME_BUDGET_SECONDS, max(0.0, remaining_work(total_deadline) - FINALIZE_BUDGET_SECONDS))
            _run_command(build_cargo_argv(), log_path=logs / "cargo-test.log", env=test_env,
                         timeout=timeout, artifact_dir=artifact_dir, cargo_artifact_monitor=True)
            validate_cargo_log(logs / "cargo-test.log")
            for case_name, contract in EXPECTED_CASES.items():
                validate_report(rust_artifacts / contract["report"], case_name, contract["sizes"])
        else:
            _require_tools(env)
            _run_local_signed(
                logs=logs, artifact_dir=artifact_dir, env=env, config_file=config_path,
                total_deadline=total_deadline, started=started, rust_artifacts=rust_artifacts,
            )
        _artifact_size(artifact_dir)
        remaining_work(total_deadline)
        status = "passed"
        message = "All three native media acceptance cases and their reports passed validation."
    except (MediaNetworkError, ui.UIE2EError, OSError, ValueError) as error:
        message = f"{type(error).__name__}: {error}"[:1024]
        print(f"[media-network-e2e] {message}", file=sys.stderr, flush=True)
    else:
        exit_code = 0
    finally:
        if work_dir is not None:
            shutil.rmtree(work_dir, ignore_errors=True)
        if artifact_dir is not None:
            try:
                _write_summary(artifact_dir, status=status, message=message, started=started)
            except (OSError, MediaNetworkError) as error:
                exit_code = 1
                print(f"[media-network-e2e] summary retention failed: {type(error).__name__}", file=sys.stderr)
    return exit_code


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-dir", help="Use this empty directory for bounded diagnostics.")
    parser.add_argument(
        "--ci-unsigned", action="store_true",
        help="Use isolated unsigned/ad-hoc toolchain outputs in credential-free CI only.",
    )
    parser.add_argument("--local-config", help="Use this public local signing config file.")
    args = parser.parse_args(argv)
    try:
        return run(args.artifact_dir, ci_unsigned=args.ci_unsigned, local_config_arg=args.local_config)
    except MediaNetworkError as error:
        print(f"[media-network-e2e] {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
