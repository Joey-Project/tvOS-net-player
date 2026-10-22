#!/usr/bin/env python3
"""Run credential-free UI playback tests against an isolated local cache server."""

from __future__ import annotations

import argparse
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
MAX_ARTIFACT_BYTES = 32 * 1024 * 1024
MAX_RESULT_BUNDLE_BYTES = 20 * 1024 * 1024
COMMAND_TIMEOUT_SECONDS = 30 * 60
SERVER_READY_TIMEOUT_SECONDS = 45
TVOS_BOOT_TIMEOUT_SECONDS = 300


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


def _signal_process_group(process: subprocess.Popen[bytes], signum: int) -> None:
    try:
        os.killpg(process.pid, signum)
    except ProcessLookupError:
        pass


def stop_process_group(
    process: subprocess.Popen[bytes], grace_seconds: float = 2.0
) -> None:
    if process.poll() is not None:
        return
    _signal_process_group(process, signal.SIGTERM)
    try:
        process.wait(timeout=grace_seconds)
        return
    except subprocess.TimeoutExpired:
        _signal_process_group(process, signal.SIGKILL)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired as error:
        raise UIE2EError("Could not reap an owned child process group.") from error


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
                if process.poll() is not None and not selector.get_map():
                    break

            if timed_out or output_limited:
                stop_process_group(process)

            if process.poll() is None:
                stop_process_group(process)
            returncode = process.wait()
            log_file.flush()
    finally:
        selector.close()
        if process is not None:
            if process.poll() is None:
                stop_process_group(process)
            if process.stdout is not None:
                process.stdout.close()

    return CommandResult(
        returncode=returncode,
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
                stop_process_group(self.process)
                break

    def stop(self) -> None:
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
        return self.process.poll() is None


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
) -> str:
    log_path = logs_dir / f"{name}.log"
    result = run_process(
        argv,
        log_path=log_path,
        timeout_seconds=timeout_seconds,
        max_output_bytes=max_output_bytes,
        env=env,
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
    app_bundle_identifier: str,
    derived_data_path: Path,
    result_bundle_path: Path,
    destination: str,
) -> list[str]:
    endpoint = validate_server_address(server_address)
    if not re.fullmatch(
        r"dev\.joey\.tvos-net-player\.uie2e\.[0-9a-f]{32}",
        app_bundle_identifier,
    ):
        raise UIE2EError("APP_BUNDLE_IDENTIFIER must be unique to this UI E2E run.")
    config = PLATFORMS[platform]
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
        "CODE_SIGNING_ALLOWED=NO",
        f"APP_BUNDLE_IDENTIFIER={app_bundle_identifier}",
        f"UI_E2E_SERVER_ADDRESS={endpoint}",
        "test",
    ]


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


def _record_tool_inventory(logs_dir: Path, runner_log: Path) -> None:
    commands = [
        ("xcode-version", ["xcodebuild", "-version"]),
        ("xcode-sdks", ["xcodebuild", "-showsdks"]),
        ("simctl-runtimes-inventory", ["xcrun", "simctl", "list", "runtimes", "--json"]),
        ("simctl-devices-inventory", ["xcrun", "simctl", "list", "devices", "available", "--json"]),
    ]
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
    name = value.rsplit("/", 1)[-1].strip()
    if name.endswith(".xctest"):
        name = name[: -len(".xctest")]
    return name


def _is_expected_target(node: dict[str, object], expected_target: str) -> bool:
    node_type = str(node.get("nodeType", "")).lower()
    if "target" not in node_type:
        return False
    return any(
        _normalized_target_name(node.get(key)) == expected_target
        for key in ("name", "nodeIdentifier", "identifier")
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
    if children:
        for child in children:
            for status, value in _test_status_counts(child).items():
                counts[status] += value
        return counts

    node_type = str(node.get("nodeType", "")).lower()
    name = str(node.get("name", ""))
    result = str(node.get("result", "")).lower()
    if result and (
        "case" in node_type
        or "method" in node_type
        or (not node_type and name.startswith("test"))
    ):
        if result in ("passed", "success"):
            counts["passed"] += 1
        elif result in ("skipped", "expected failure"):
            counts["skipped"] += 1
        else:
            counts["failed"] += 1
    return counts


def validate_xcresult(
    summary: object, tests: object, expected_target: str
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
    return overall | {f"target_{key}": value for key, value in target_counts.items()}


class ArtifactStager:
    """Copy only an explicit small allowlist, never fixture or build directories."""

    ALLOWED_FILES = {
        "runner.log",
        "server.log",
        "xcodebuild.log",
        "xcresult-summary.json",
        "xcresult-tests.json",
        "result-validation.json",
    }

    def __init__(self, destination: Path, max_bytes: int = MAX_ARTIFACT_BYTES) -> None:
        self.destination = destination
        self.max_bytes = max_bytes
        self.used_bytes = 0
        self.destination.mkdir(parents=True, exist_ok=True, mode=0o700)
        if any(self.destination.iterdir()):
            raise UIE2EError(
                f"Artifact directory must be empty before use: {self.destination}"
            )

    def stage_file(self, source: Path) -> None:
        if source.name not in self.ALLOWED_FILES:
            raise UIE2EError(f"Artifact file is not allowlisted: {source.name}")
        if not source.is_file() or source.is_symlink():
            return
        size = source.stat().st_size
        if size > MAX_COMMAND_LOG_BYTES or self.used_bytes + size > self.max_bytes:
            return
        shutil.copyfile(source, self.destination / source.name)
        self.used_bytes += size

    def stage_result_bundle(self, source: Path) -> bool:
        if not source.is_dir() or source.is_symlink():
            return False
        staged = self.destination / "result.xcresult"
        temp = self.destination / f".result-{uuid.uuid4().hex}.tmp"
        count = 0
        try:
            temp.mkdir(mode=0o700)
            for current, directories, files in os.walk(source, followlinks=False):
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
                                or self.used_bytes + count > self.max_bytes
                            ):
                                shutil.rmtree(temp)
                                return False
                            writer.write(chunk)
            temp.rename(staged)
            self.used_bytes += count
            return True
        except Exception:
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
    for source_name in (
        "runner.log",
        "server.log",
        "xcodebuild.log",
        "xcresult-summary.json",
        "xcresult-tests.json",
        "result-validation.json",
    ):
        source = work_dir / "logs" / source_name
        if source.exists():
            stager.stage_file(source)
    stager.stage_result_bundle(result_bundle)


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
            _record_tool_inventory(logs_dir, runner_log)

            scheme_path = (
                ROOT
                / "TVOSNetPlayer.xcodeproj"
                / "xcshareddata"
                / "xcschemes"
                / f"{config.scheme}.xcscheme"
            )
            verify_scheme_contract(scheme_path, config.test_target)
            log(f"validated shared scheme and endpoint mapping: {config.scheme}")

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
                app_bundle_identifier=(
                    f"dev.joey.tvos-net-player.uie2e.{uuid.uuid4().hex}"
                ),
                derived_data_path=derived_data_path,
                result_bundle_path=result_bundle,
                destination=destination,
            )
            child_env = os.environ.copy()
            child_env["UI_E2E_SERVER_ADDRESS"] = endpoint
            xcode_log = logs_dir / "xcodebuild.log"
            log("starting actual xcodebuild test execution.")
            xcode_result = run_process(
                command,
                log_path=xcode_log,
                timeout_seconds=COMMAND_TIMEOUT_SECONDS,
                max_output_bytes=MAX_COMMAND_LOG_BYTES,
                env=child_env,
                cwd=ROOT,
            )
            if xcode_result.timed_out:
                raise UIE2EError(
                    f"xcodebuild test exceeded its {COMMAND_TIMEOUT_SECONDS // 60}-minute deadline."
                )
            if xcode_result.output_limited:
                raise UIE2EError(
                    f"xcodebuild test exceeded its {MAX_COMMAND_LOG_BYTES // (1024 * 1024)} MiB log limit."
                )
            if xcode_result.returncode != 0:
                raise UIE2EError(
                    f"xcodebuild test exited {xcode_result.returncode}; "
                    "inspect the bounded xcodebuild log and xcresult."
                )
            if not result_bundle.is_dir():
                raise UIE2EError("xcodebuild exited 0 without creating an xcresult bundle.")

            summary_path = logs_dir / "xcresult-summary.json"
            summary_result = run_process(
                build_xcresulttool_command("summary", result_bundle),
                log_path=summary_path,
                timeout_seconds=120,
                max_output_bytes=MAX_RESULT_SUMMARY_BYTES,
                cwd=ROOT,
            )
            _require_result(summary_result, "xcresult summary", summary_path)
            tests_path = logs_dir / "xcresult-tests.json"
            tests_result = run_process(
                build_xcresulttool_command("tests", result_bundle),
                log_path=tests_path,
                timeout_seconds=120,
                max_output_bytes=MAX_RESULT_SUMMARY_BYTES,
                cwd=ROOT,
            )
            _require_result(tests_result, "xcresult test tree", tests_path)
            counts = validate_xcresult(
                _read_json_file(summary_path),
                _read_json_file(tests_path),
                config.test_target,
            )
            _write_json(logs_dir / "result-validation.json", counts)
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
