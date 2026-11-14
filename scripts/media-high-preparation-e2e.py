#!/usr/bin/env python3
"""Run the opt-in, credential-free native high-preparation comparison."""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import os
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent
DEFAULT_ARTIFACT_ROOT = ROOT / "build" / "MediaHighPreparationE2E" / "artifacts"
TOTAL_BUDGET_SECONDS = 60 * 60
COMPILE_BUDGET_SECONDS = 25 * 60
RUNTIME_BUDGET_SECONDS = 30 * 60
FINALIZE_BUDGET_SECONDS = 60
FIXTURE_GENERATION_TIMEOUT_SECONDS = 240
ARTIFACT_BYTES = 32 * 1024 * 1024
COMMAND_LOG_BYTES = 8 * 1024 * 1024
REPORT_BYTES = 64 * 1024
SUMMARY_BYTES = 16 * 1024
OUTPUT_RESERVE_BYTES = SUMMARY_BYTES + 2 * REPORT_BYTES + 4 * 1024
MAX_ARTIFACT_ENTRIES = 256
MAX_PLAN_RECORDS = 16
MAX_SERIALIZED_PLAN_RECORDS = 1
MAX_SELECTED_SPANS = 4
CAUSAL_EVENT_RING_LIMIT = 96
TEST_NAME = "media_high_preparation_native_tests::native_high_preparation_off_on_comparison"
PROBE_SELF_TEST_CASE = "high_preparation_player_verification_survives_cleanup"
ARM_REPORTS = {"off": "off.json", "on": "on.json"}
TRANSPORT_BOUNDARY = "in_process_generated_cache_service_trait"
NEWLY_READABLE_SEMANTICS = "one_byte_lower_bound_per_exact_range_none_to_full_some_transition"


class HighPreparationError(RuntimeError):
    pass


def _load_module(name: str, path: Path) -> Any:
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise HighPreparationError(f"Could not load the {name} helper.")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def default_artifact_root(root: Path = ROOT) -> Path:
    return root / "build" / "MediaHighPreparationE2E" / "artifacts"


def resolve_artifact_root(value: str | None, *, root: Path = ROOT) -> Path:
    if value is None:
        return default_artifact_root(root)
    path = Path(value).expanduser()
    if not path.is_absolute():
        raise HighPreparationError("--artifact-dir must be an absolute path.")
    return path


def _ensure_owned_directory(path: Path) -> Path:
    if not path.is_absolute():
        raise HighPreparationError("Artifact root must be an absolute path.")
    if ".." in path.parts:
        raise HighPreparationError("Artifact root must not contain parent traversal.")
    current = Path(path.anchor)
    for component in path.parts[1:]:
        current = current / component
        try:
            info = current.lstat()
        except FileNotFoundError:
            try:
                current.mkdir(mode=0o700)
            except FileExistsError:
                pass
            try:
                info = current.lstat()
            except OSError as error:
                raise HighPreparationError("Artifact root could not be created safely.") from error
        except OSError as error:
            raise HighPreparationError("Artifact root could not be inspected safely.") from error
        if current.is_symlink() or not current.is_dir():
            raise HighPreparationError("Artifact root path contains a symlink or non-directory.")
        if current == path and info.st_uid != os.getuid():
            raise HighPreparationError("Artifact root is not owned by the current user.")
    return path


def _private_run_directory(artifact_root: Path | None = None) -> Path:
    root = _ensure_owned_directory((artifact_root or default_artifact_root()).expanduser())
    runs = root / "runs"
    try:
        runs.mkdir(mode=0o700)
    except FileExistsError:
        info = runs.lstat()
        if runs.is_symlink() or not runs.is_dir() or info.st_uid != os.getuid():
            raise HighPreparationError("Artifact run directory is not owned and regular.")
    path = runs / uuid.uuid4().hex
    path.mkdir(mode=0o700)
    return path


def artifact_usage(directory: Path) -> tuple[int, int]:
    total = 0
    entries = 0
    for current, dirs, files in os.walk(directory, followlinks=False):
        base = Path(current)
        entries += len(dirs) + len(files)
        if entries > MAX_ARTIFACT_ENTRIES:
            raise HighPreparationError("Artifact tree exceeded its entry-count bound.")
        for name in dirs + files:
            item = base / name
            if item.is_symlink():
                raise HighPreparationError("Artifact tree contains a symlink.")
        for name in files:
            item = base / name
            if not item.is_file():
                raise HighPreparationError("Artifact tree contains a non-regular file.")
            total += item.stat().st_size
            if total > ARTIFACT_BYTES:
                raise HighPreparationError("Artifact tree exceeded its 32 MiB bound.")
    return total, entries


def _clean_environment() -> dict[str, str]:
    allowed = (
        "PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
        "TERM", "DEVELOPER_DIR", "SDKROOT", "TOOLCHAINS", "SWIFT_EXEC", "CARGO_HOME",
        "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR",
    )
    result = {key: os.environ[key] for key in allowed if key in os.environ}
    result.setdefault("PATH", os.defpath)
    return result


def _run_bounded(
    argv: list[str], *, log_path: Path, env: dict[str, str], timeout_seconds: float,
    max_output_bytes: int,
) -> tuple[int, str]:
    ui = _load_module("media_high_preparation_ui", ROOT / "scripts" / "ui-e2e.py")
    artifact_dir = log_path.parent.parent
    used_bytes, _ = artifact_usage(artifact_dir)
    remaining = ARTIFACT_BYTES - OUTPUT_RESERVE_BYTES - used_bytes
    if remaining <= 0:
        raise HighPreparationError("No artifact budget remains for bounded command output.")
    result = ui.run_process(
        argv,
        log_path=log_path,
        timeout_seconds=max(0.1, timeout_seconds),
        max_output_bytes=min(max_output_bytes, remaining),
        env=env,
        cwd=ROOT,
    )
    if result.timed_out:
        raise HighPreparationError(f"Bounded command timed out: {log_path.name}")
    if result.output_limited:
        raise HighPreparationError(f"Bounded command exceeded its output cap: {log_path.name}")
    return int(result.returncode), log_path.read_text(encoding="utf-8", errors="replace")


def _verify_native_tools(env: dict[str, str]) -> None:
    if sys.platform != "darwin":
        raise HighPreparationError("Native AVPlayer comparison is available only on macOS.")
    missing = [name for name in ("ffmpeg", "ffprobe", "osascript") if shutil.which(name, path=env["PATH"]) is None]
    if missing:
        raise HighPreparationError("Required local native media tools are unavailable: " + ", ".join(missing))


def _listed_test_names(output: str) -> list[str]:
    suffix = ": test"
    return [line[:-len(suffix)] for line in output.splitlines() if line.endswith(suffix)]


def _verify_test_filter(test_binary: Path, *, logs: Path, env: dict[str, str], deadline: float) -> None:
    code, output = _run_bounded(
        [str(test_binary), TEST_NAME, "--exact", "--ignored", "--list"],
        log_path=logs / "high-preparation-test-list.log",
        env=env,
        timeout_seconds=min(60, deadline - time.monotonic()),
        max_output_bytes=64 * 1024,
    )
    listed = _listed_test_names(output)
    if code != 0 or listed != [TEST_NAME]:
        raise HighPreparationError("Stable test binary does not contain exactly the ignored opt-in test.")


def _verify_legacy_native_test_filter(
    test_binary: Path, *, network: Any, logs: Path, env: dict[str, str], deadline: float,
) -> None:
    code, output = _run_bounded(
        [
            str(test_binary), "media_acceptance_tests::", "--ignored", "--list",
            "--skip", network.EXTENDED_DIAGNOSTIC_TEST,
        ],
        log_path=logs / "legacy-native-test-list.log",
        env=env,
        timeout_seconds=min(60, deadline - time.monotonic()),
        max_output_bytes=64 * 1024,
    )
    listed = _listed_test_names(output)
    expected = set(network.EXPECTED_TESTS)
    if code != 0 or len(listed) != len(expected) or set(listed) != expected:
        raise HighPreparationError("Legacy ignored-test filter must contain exactly its three existing native cases.")


def _verify_probe_self_test(probe_binary: Path, *, logs: Path, env: dict[str, str], deadline: float) -> None:
    code, output = _run_bounded(
        [str(probe_binary), "--self-test"],
        log_path=logs / "probe-self-test.log",
        env=env,
        timeout_seconds=min(60, deadline - time.monotonic()),
        max_output_bytes=64 * 1024,
    )
    try:
        value = json.loads(output)
    except json.JSONDecodeError as error:
        raise HighPreparationError("Probe self-test did not return JSON.") from error
    required_cases = {
        "high_preparation_mode_requires_bound_reader_and_fixed_phase_limits",
        PROBE_SELF_TEST_CASE,
    }
    if code != 0 or value.get("status") != "complete" or not required_cases.issubset(value.get("cases", [])):
        raise HighPreparationError("Probe self-test did not validate the continuous-only mode.")


def _load_network_runner() -> Any:
    return _load_module("media_high_preparation_network_runner", ROOT / "scripts" / "media-network-e2e.py")


def _build_and_run_locked(
    *, config_file: Path | None, logs: Path, artifact_dir: Path, reports: Path,
    env: dict[str, str], started: float, total_deadline: float,
) -> tuple[Path, Path, dict[str, str], float]:
    local_build = _load_module("media_high_preparation_local_build", ROOT / "scripts" / "local_build.py")
    network = _load_network_runner()
    config = local_build.load_config(config_file)
    compile_deadline = min(started + COMPILE_BUDGET_SECONDS, total_deadline - RUNTIME_BUDGET_SECONDS - FINALIZE_BUDGET_SECONDS)
    build_started = time.monotonic()
    invocation = 0
    original_run = local_build.RUN

    def bounded_build_run(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
        nonlocal invocation
        invocation += 1
        timeout_seconds = compile_deadline - time.monotonic()
        if timeout_seconds <= 0:
            raise local_build.BuildError("high-preparation build exceeded its 25-minute compile budget")
        available = ARTIFACT_BYTES - OUTPUT_RESERVE_BYTES - network._artifact_size(artifact_dir)
        limit = min(COMMAND_LOG_BYTES, available)
        if limit <= 0:
            raise local_build.BuildError("high-preparation artifact budget is exhausted")
        log_path = logs / f"signed-build-{invocation}.log"
        command_env = kwargs.get("env", env)
        command_env = network.clean_environment(command_env) if isinstance(command_env, dict) else env
        command_cwd = kwargs.get("cwd", ROOT)
        result = network.ui.run_process(
            list(argv), log_path=log_path, timeout_seconds=timeout_seconds,
            max_output_bytes=limit, env=command_env, cwd=Path(command_cwd),
        )
        if result.timed_out:
            raise local_build.BuildError(f"signed build exceeded its shared deadline; log: {log_path.name}")
        if result.output_limited:
            raise local_build.BuildError(f"signed build exceeded its output cap; log: {log_path.name}")
        output = log_path.read_text(encoding="utf-8", errors="replace")
        if result.returncode != 0 and kwargs.get("check", True):
            raise local_build.BuildError(f"signed build exited {result.returncode}; log: {log_path.name}")
        return subprocess.CompletedProcess(list(argv), result.returncode, output, "")

    try:
        local_build.RUN = bounded_build_run
        with local_build.prepared_targets({"probe", "media-tests"}, config_file=config_file) as receipt:
            compile_seconds = time.monotonic() - build_started
            if compile_seconds > COMPILE_BUDGET_SECONDS:
                raise HighPreparationError("Pinned product preparation exceeded its 25-minute compile budget.")
            probe, test_binary, identities = _products_from_receipt(local_build, receipt, config)
            _verify_legacy_native_test_filter(
                test_binary, network=network, logs=logs, env=env, deadline=total_deadline,
            )
            _verify_test_filter(test_binary, logs=logs, env=env, deadline=total_deadline)
            _verify_probe_self_test(probe, logs=logs, env=env, deadline=total_deadline)
            mute_and_verify_system_audio(logs=logs, env=env, deadline=total_deadline)
            test_env = dict(env)
            test_env["LAN_MEDIA_ACCEPTANCE_PROBE_BINARY"] = str(probe)
            test_env["LAN_MEDIA_HIGH_PREPARATION_ARTIFACTS"] = str(reports)
            test_env["LAN_MEDIA_HIGH_PREPARATION_SYSTEM_MUTED"] = "verified"
            argv = [
                str(test_binary), TEST_NAME, "--exact", "--ignored", "--nocapture",
                "--test-threads=1", "--color=never",
            ]
            runtime_deadline = min(total_deadline - FINALIZE_BUDGET_SECONDS, time.monotonic() + RUNTIME_BUDGET_SECONDS)
            log_path = logs / "native-comparison.log"
            previous_artifact_limit = network.ARTIFACT_BYTES
            previous_artifact_size = network._artifact_size

            def bounded_artifact_size(directory: Path, *, limit: int = ARTIFACT_BYTES) -> int:
                try:
                    size, _ = artifact_usage(directory)
                except HighPreparationError as error:
                    raise network.MediaNetworkError(str(error)) from error
                if size > limit:
                    raise network.MediaNetworkError("Retained artifact output exceeded its byte bound.")
                return size

            network._artifact_size = bounded_artifact_size
            network.ARTIFACT_BYTES = min(previous_artifact_limit, ARTIFACT_BYTES - OUTPUT_RESERVE_BYTES)
            try:
                code = network._run_command(
                    argv, log_path=log_path, env=test_env,
                    timeout=runtime_deadline - time.monotonic(), artifact_dir=artifact_dir,
                    cargo_artifact_monitor=True,
                ).returncode
            finally:
                network.ARTIFACT_BYTES = previous_artifact_limit
                network._artifact_size = previous_artifact_size
            if code != 0:
                raise HighPreparationError("Opt-in native comparison test failed; see retained bounded log.")
            return probe, test_binary, identities, compile_seconds
    except local_build.BuildError as error:
        raise HighPreparationError("Pinned stable build or signing verification failed.") from error
    finally:
        local_build.RUN = original_run


def _products_from_receipt(
    local_build: Any, receipt: dict[str, Any], config: dict[str, Any],
) -> tuple[Path, Path, dict[str, str]]:
    root = config["durable_root"]
    products: dict[str, Path] = {}
    identities: dict[str, str] = {}
    for target in ("probe", "media-tests"):
        item = receipt.get("products", {}).get(target)
        if not isinstance(item, dict):
            raise HighPreparationError("Pinned build receipt omitted a required stable product.")
        path = Path(str(item.get("path", "")))
        expected = root / local_build.PRODUCTS[target]
        if path != expected or path.is_symlink() or not path.is_file():
            raise HighPreparationError("Pinned build returned a non-stable executable path.")
        details = local_build.verify_product(target, path, config, item)
        if any(details.get(key) != item.get(key) for key in (
            "identifier", "team_id", "certificate_sha1", "designated_requirement", "cdhash",
        )):
            raise HighPreparationError("Pinned executable signature did not match its receipt.")
        products[target] = path
        identities[target] = str(item["cdhash"])
    return products["probe"], products["media-tests"], identities


def mute_and_verify_system_audio(*, logs: Path, env: dict[str, str], deadline: float) -> None:
    for name, expression, expected in (
        ("system-mute-set.log", "set volume output muted true", None),
        ("system-mute-verify.log", "output muted of (get volume settings)", "true"),
    ):
        code, output = _run_bounded(
            ["osascript", "-e", expression],
            log_path=logs / name,
            env=env,
            timeout_seconds=min(10, deadline - time.monotonic()),
            max_output_bytes=4_096,
        )
        if code != 0 or (expected is not None and output.strip().lower() != expected):
            raise HighPreparationError("System audio mute could not be set and verified.")


def _read_arm_report(path: Path) -> dict[str, Any]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise HighPreparationError(f"Missing bounded arm report: {path.name}") from error
    if len(raw) > REPORT_BYTES:
        raise HighPreparationError(f"Arm report exceeded {REPORT_BYTES} bytes: {path.name}")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise HighPreparationError(f"Arm report was not valid JSON: {path.name}") from error
    validate_arm_report(value, expected_arm=path.stem)
    return value


def validate_arm_report(value: Any, *, expected_arm: str) -> None:
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise HighPreparationError("Arm report schema is invalid.")
    if expected_arm not in ARM_REPORTS or value.get("arm") != expected_arm:
        raise HighPreparationError("Arm report identity is invalid.")
    if value.get("transport_boundary") != TRANSPORT_BOUNDARY:
        raise HighPreparationError("Arm report used an unapproved transport boundary.")
    if value.get("status") not in ("complete", "inconclusive") or type(value.get("eligible")) is not bool:
        raise HighPreparationError("Arm report status is invalid.")
    if expected_arm == "off" and value.get("status") != "complete":
        raise HighPreparationError("OFF must complete its full automatic native phase.")
    if expected_arm == "on" and value["eligible"] != (value.get("status") == "complete"):
        raise HighPreparationError("ON eligibility and explicit inconclusive status disagree.")
    if value.get("fixture_is_fresh") is not True or value.get("cache_is_fresh") is not True:
        raise HighPreparationError("Each arm must use an independent fresh fixture and cache.")
    fixture_generation_timeout = value.get("fixture_generation_timeout_seconds")
    if type(fixture_generation_timeout) is not int or fixture_generation_timeout != FIXTURE_GENERATION_TIMEOUT_SECONDS:
        raise HighPreparationError("Arm report fixture-generation timeout differs from the approved opt-in limit.")
    reader = value.get("reader")
    if not isinstance(reader, dict) or not isinstance(reader.get("id"), str):
        raise HighPreparationError("Arm report is missing its native reader binding.")
    if not _is_uuid4(reader["id"]) or not all(isinstance(reader.get(key), str) and reader[key] for key in ("context_id", "session_id")):
        raise HighPreparationError("Arm report context/reader binding is invalid.")
    if type(reader.get("session_generation")) is not int or reader["session_generation"] <= 0:
        raise HighPreparationError("Arm report session generation is invalid.")
    native = value.get("native")
    if not isinstance(native, dict) or native.get("mode") != "continuous_native_control_v1":
        raise HighPreparationError("Arm report did not use the continuous-only native mode.")
    if (native.get("requested_duration_seconds"), native.get("deadline_seconds")) != (600, 720):
        raise HighPreparationError("Arm report phase bounds differ from the approved profile.")
    if native.get("phase_completed") is not True:
        raise HighPreparationError("Native automatic phase did not run to its bounded completion.")
    if type(native.get("challenge_sample_count")) is not int or not 0 <= native["challenge_sample_count"] <= 1024:
        raise HighPreparationError("Native challenge sampling exceeded its response bound.")
    if native.get("challenge_samples_truncated") is not False:
        raise HighPreparationError("Native challenge response records were truncated.")
    if native.get("muted_verified") is not True or native.get("automatic_waiting_enabled") is not True:
        raise HighPreparationError("Arm report did not verify mute and default automatic buffering.")
    if native.get("system_audio_mute_verified") is not True:
        raise HighPreparationError("Arm report did not bind playback to verified system mute.")
    if any(native.get(key) != expected for key, expected in (
        ("seek_count", 0), ("rate_change_count", 0), ("item_replacement_count", 0), ("buffer_cap_set", False),
    )):
        raise HighPreparationError("Measured playback modified a prohibited native control.")
    decoded = native.get("high_low_high_decoded")
    decode_censored = native.get("decode_censored")
    if type(decoded) is not bool or type(decode_censored) is not bool \
        or decode_censored != (not decoded and value["eligible"] is True):
        raise HighPreparationError("Decode censoring is permitted only with otherwise eligible preparation.")
    plans = value.get("plans")
    if not isinstance(plans, list) or len(plans) > MAX_SERIALIZED_PLAN_RECORDS:
        raise HighPreparationError("Plan evidence is missing or exceeds its record bound.")
    for plan in plans:
        if not isinstance(plan, dict) or not isinstance(plan.get("selected_spans"), list) or len(plan["selected_spans"]) > MAX_SELECTED_SPANS:
            raise HighPreparationError("Plan evidence is malformed or exceeds its span bound.")
        if "downloaded_bytes" in plan or "origin_attempts" in plan:
            raise HighPreparationError("Logical plan observations were mislabeled as transfer evidence.")
    plan_retention = value.get("plan_history_retention")
    if not isinstance(plan_retention, dict) or any(
        type(plan_retention.get(key)) is not int
        for key in (
            "observed_matching_candidates", "internally_retained_records", "internal_record_limit",
            "serialized_plan_records", "serialization_record_limit", "producer_history_limit",
        )
    ) or type(plan_retention.get("truncated")) is not bool:
        raise HighPreparationError("Plan history retention metadata is missing or malformed.")
    if plan_retention["observed_matching_candidates"] < plan_retention["internally_retained_records"] \
        or plan_retention["internally_retained_records"] > plan_retention["internal_record_limit"] \
        or plan_retention["internal_record_limit"] != MAX_PLAN_RECORDS \
        or plan_retention["serialized_plan_records"] != len(plans) \
        or plan_retention["serialization_record_limit"] != MAX_SERIALIZED_PLAN_RECORDS \
        or plan_retention["producer_history_limit"] != MAX_PLAN_RECORDS:
        raise HighPreparationError("Plan history retention counts do not match retained evidence.")
    if value["eligible"] and plan_retention["truncated"]:
        raise HighPreparationError("Truncated matching plan history cannot establish eligibility.")
    for key in ("origin_range_transfers", "player_range_transfers"):
        events = value.get(key)
        if not isinstance(events, list) or len(events) > MAX_SELECTED_SPANS:
            raise HighPreparationError(f"Arm report {key} is missing or exceeds its record bound.")
    transfer_retention = value.get("transfer_event_retention")
    if not isinstance(transfer_retention, dict):
        raise HighPreparationError("Transfer event retention metadata is missing.")
    for name, events in (("origin", value["origin_range_transfers"]), ("player", value["player_range_transfers"])):
        fields = (
            f"{name}_recovery_range_events_observed",
            f"{name}_witnesses_retained",
            f"{name}_ring_limit",
        )
        if any(type(transfer_retention.get(field)) is not int for field in fields) \
            or type(transfer_retention.get(f"{name}_ring_truncated")) is not bool:
            raise HighPreparationError(f"{name.title()} transfer retention metadata is malformed.")
        if transfer_retention[fields[0]] < transfer_retention[fields[1]] \
            or transfer_retention[fields[1]] != len(events) \
            or transfer_retention[fields[2]] != CAUSAL_EVENT_RING_LIMIT:
            raise HighPreparationError(f"{name.title()} transfer retention counts do not match bounded witnesses.")
    first_body = value.get("first_recovery_high_completed_source_body")
    if first_body is not None:
        if not isinstance(first_body, dict):
            raise HighPreparationError("First recovery high source-body evidence is malformed.")
        start, end, observed = (
            first_body.get("range_start"),
            first_body.get("range_end_exclusive"),
            first_body.get("bytes_observed"),
        )
        started = first_body.get("request_started_monotonic_seconds")
        completed = first_body.get("completed_monotonic_seconds")
        if first_body.get("status_code") != 206 or first_body.get("body_complete") is not True \
            or not all(type(number) is int for number in (start, end, observed)) \
            or end <= start or observed != end - start \
            or type(started) not in (int, float) or type(completed) not in (int, float) \
            or not math.isfinite(started) or not math.isfinite(completed) or completed < started:
            raise HighPreparationError("First recovery high body was not proven complete at its exact range length.")
    first_decoded = value.get("first_recovery_high_decoded_monotonic_seconds")
    if first_decoded is not None and (
        type(first_decoded) not in (int, float) or not math.isfinite(first_decoded) or first_decoded <= 0
    ):
        raise HighPreparationError("First recovery high decode timestamp is invalid.")
    logical = value.get("logical_range_counts")
    if not isinstance(logical, dict) or any(
        type(logical.get(key)) is not int or logical[key] < 0
        for key in ("requested_resource_ranges", "readable_resource_ranges", "deferred_resource_ranges")
    ):
        raise HighPreparationError("Arm report omitted the named logical resource-range counters.")
    if "downloaded_bytes" in logical or "origin_attempts" in logical:
        raise HighPreparationError("Logical coordinator counters were mislabeled as transfer evidence.")
    attribution = value.get("attribution")
    if not isinstance(attribution, dict) or attribution.get("exclusive_attribution") != "not_proven":
        raise HighPreparationError("Arm report must preserve the exclusive-attribution limitation.")
    if attribution.get("causal_faster_selection_claim") is not False:
        raise HighPreparationError("Arm report must not claim causal faster selection.")
    if value["eligible"]:
        evidence = value.get("recovery_preparation")
        if not isinstance(evidence, dict) or evidence.get("completed") is not True \
            or evidence.get("initial_window_readable") is not False or evidence.get("window_readable") is not True:
            raise HighPreparationError("Eligible ON evidence requires an initially incomplete, finally readable window.")
        if evidence.get("initial_missing_to_final_readable") is not True:
            raise HighPreparationError("Eligible ON evidence requires measured missing-to-readable byte coverage.")
        if evidence.get("newly_readable_semantics") != NEWLY_READABLE_SEMANTICS:
            raise HighPreparationError("Eligible ON evidence omitted the approved one-byte lower-bound semantics.")
        lower_bound = evidence.get("newly_readable_bytes_lower_bound")
        if type(lower_bound) is not int or lower_bound <= 0:
            raise HighPreparationError("Eligible ON evidence requires a positive exact-range lower bound.")
        if evidence.get("completed_origin_range_overlaps") is not True:
            raise HighPreparationError("Eligible ON evidence requires completed real origin range activity.")
        if evidence.get("player_range_overlap_status") != "yes":
            raise HighPreparationError("Eligible ON evidence requires a matching player range overlap.")
        spans = evidence.get("selected_spans")
        if not isinstance(spans, list) or not spans:
            raise HighPreparationError("Eligible ON evidence must include the selected byte spans.")
        if not {"Video", "Audio"}.issubset({span.get("kind") for span in spans if isinstance(span, dict)}):
            raise HighPreparationError("Eligible ON evidence must retain selected video and audio ranges.")
        plan_id = evidence.get("plan_id")
        matching_plans = [plan for plan in plans if plan.get("plan_id") == plan_id]
        if not any(
            plan.get("outcome") == "Completed"
            and plan.get("initial_window_readable") is False
            and plan.get("window_readable") is True
            and plan.get("context_id") == reader["context_id"]
            and plan.get("reader_id") == reader["id"]
            and plan.get("session_id") == reader["session_id"]
            and plan.get("session_generation") == reader["session_generation"]
            for plan in matching_plans
        ):
            raise HighPreparationError("Eligible ON evidence must retain its completed coordinator plan.")
        if not any(plan.get("selected_spans") == spans for plan in matching_plans):
            raise HighPreparationError("Selected recovery spans do not match the retained coordinator plan.")
        lower_bound_sum = 0
        for span in spans:
            if not isinstance(span, dict):
                raise HighPreparationError("Selected span evidence is malformed.")
            start = span.get("byte_range_start")
            end = span.get("byte_range_end_exclusive")
            added = span.get("newly_readable_bytes_lower_bound")
            before_state = span.get("before_read_state")
            if not all(type(number) is int for number in (start, end, added)) or end <= start:
                raise HighPreparationError("Selected SIDX byte range is invalid.")
            if before_state not in ("missing", "readable", "unknown") \
                or span.get("after_read_state") != "readable" or span.get("readable") is not True:
                raise HighPreparationError("Eligible ON evidence did not prove every selected A/V range readable.")
            expected_lower_bound = 1 if before_state == "missing" else 0
            if added != expected_lower_bound:
                raise HighPreparationError("Unknown or already-readable cache state was recast as a missing-byte delta.")
            lower_bound_sum += added
            if not _span_matches_reader_window(span, reader, evidence):
                raise HighPreparationError("Selected span identity does not match its context, generation, variant, or window.")
        if lower_bound_sum != lower_bound:
            raise HighPreparationError("Recovery lower bound did not match exact selected-range transitions.")
        origin_events = value["origin_range_transfers"]
        if not any(
            _completed_origin_overlap(event, spans, reader, evidence)
            for event in origin_events if isinstance(event, dict)
        ):
            raise HighPreparationError("Eligible ON evidence lacks a completed origin body overlapping selected bytes.")
        player_events = value["player_range_transfers"]
        if not any(
            _player_ingress_overlap(event, spans, reader, evidence)
            for event in player_events if isinstance(event, dict)
        ):
            raise HighPreparationError("Eligible ON evidence lacks matching reader-bound player ingress overlap.")


def _is_uuid4(value: str) -> bool:
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError):
        return False
    return str(parsed) == value and parsed.version == 4


def _ranges_overlap(left: dict[str, Any], right: dict[str, Any]) -> bool:
    try:
        left_start, left_end = left["range_start"], left["range_end_exclusive"]
        right_start, right_end = right["byte_range_start"], right["byte_range_end_exclusive"]
    except (KeyError, TypeError):
        return False
    return all(type(value) is int for value in (left_start, left_end, right_start, right_end)) \
        and left_start < right_end and right_start < left_end


def _span_matches_reader_window(
    span: dict[str, Any], reader: dict[str, Any], evidence: dict[str, Any],
) -> bool:
    return (
        span.get("context_id") == reader.get("context_id")
        and span.get("session_id") == reader.get("session_id")
        and span.get("session_generation") == reader.get("session_generation")
        and span.get("kind") in ("Video", "Audio")
        and isinstance(span.get("variant_id"), str) and bool(span["variant_id"])
        and isinstance(span.get("resource_id"), str) and bool(span["resource_id"])
        and span.get("logical_window_start_seconds") == evidence.get("window_start_seconds")
        and span.get("logical_window_end_seconds") == evidence.get("window_end_seconds")
    )


def _event_matches_span(
    event: dict[str, Any], span: dict[str, Any], reader: dict[str, Any], evidence: dict[str, Any],
) -> bool:
    return (
        event.get("context_id") == reader.get("context_id")
        and event.get("reader_id") == reader.get("id")
        and event.get("session_id") == reader.get("session_id")
        and event.get("session_generation") == reader.get("session_generation")
        and event.get("variant_id") == span.get("variant_id")
        and event.get("resource_id") == span.get("resource_id")
        and event.get("logical_window_start_seconds") == evidence.get("window_start_seconds")
        and event.get("logical_window_end_seconds") == evidence.get("window_end_seconds")
        and type(event.get("request_started_monotonic_seconds")) in (int, float)
        and math.isfinite(event["request_started_monotonic_seconds"])
        and event["request_started_monotonic_seconds"] >= evidence.get("admitted_sample_monotonic_seconds", float("inf"))
        and _ranges_overlap(event, span)
    )


def _completed_origin_overlap(
    event: dict[str, Any], spans: list[Any], reader: dict[str, Any], evidence: dict[str, Any],
) -> bool:
    start = event.get("range_start")
    end = event.get("range_end_exclusive")
    observed = event.get("bytes_observed")
    started = event.get("request_started_monotonic_seconds")
    completed = event.get("completed_monotonic_seconds")
    if event.get("status_code") != 206 or event.get("body_complete") is not True:
        return False
    if not all(type(value) is int for value in (start, end, observed)) or end <= start or observed != end - start:
        return False
    if type(started) not in (int, float) or type(completed) not in (int, float) \
        or not math.isfinite(started) or not math.isfinite(completed) or completed < started:
        return False
    return any(
        _event_matches_span(event, span, reader, evidence)
        for span in spans if isinstance(span, dict)
    )


def _player_ingress_overlap(
    event: dict[str, Any], spans: list[Any], reader: dict[str, Any], evidence: dict[str, Any],
) -> bool:
    start = event.get("range_start")
    end = event.get("range_end_exclusive")
    if event.get("status_code") != 206 or not all(type(value) is int for value in (start, end)) or end <= start:
        return False
    return any(
        _event_matches_span(event, span, reader, evidence)
        for span in spans if isinstance(span, dict)
    )


def validate_comparison(off: dict[str, Any], on: dict[str, Any]) -> dict[str, Any]:
    validate_arm_report(off, expected_arm="off")
    validate_arm_report(on, expected_arm="on")
    if off["reader"]["id"] == on["reader"]["id"] or off["reader"]["session_id"] == on["reader"]["session_id"]:
        raise HighPreparationError("OFF and ON must use distinct fresh reader/session bindings.")
    if off["eligible"]:
        raise HighPreparationError("OFF cannot claim new-coordinator preparation eligibility.")
    comparison_status = "eligible" if off["status"] == "complete" and on["eligible"] else "inconclusive"
    return {
        "schema_version": 1,
        "status": "complete",
        "comparison_status": comparison_status,
        "arms": {"off": off["status"], "on": on["status"]},
        "exclusive_attribution": "not_proven",
        "causal_faster_selection_claim": False,
        "latency_negative_evidence": False,
    }


def comparison_exit_status(comparison_status: str) -> tuple[str, int]:
    if comparison_status == "eligible":
        return "complete", 0
    if comparison_status == "inconclusive":
        return "inconclusive", 2
    raise HighPreparationError("Comparison status is not recognized.")


def _atomic_write_json(path: Path, value: dict[str, Any], maximum_bytes: int) -> None:
    encoded = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
    if len(encoded) > maximum_bytes:
        raise HighPreparationError("Final summary exceeded its byte bound.")
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("xb") as stream:
        stream.write(encoded)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def run(*, enabled: bool, config_file: Path | None = None, artifact_root: Path | None = None) -> int:
    if not enabled:
        raise HighPreparationError("This runner is opt-in; pass --run explicitly.")
    started = time.monotonic()
    total_deadline = started + TOTAL_BUDGET_SECONDS
    run_dir = _private_run_directory(artifact_root)
    logs = run_dir / "logs"
    reports = run_dir / "reports"
    logs.mkdir(mode=0o700)
    reports.mkdir(mode=0o700)
    status = "failed"
    exit_code = 1
    comparison_status: str | None = None
    finalize_started: float | None = None
    summary: dict[str, Any] = {"schema_version": 1, "status": status}
    try:
        env = _clean_environment()
        _verify_native_tools(env)
        _probe_binary, _test_binary, identities, compile_seconds = _build_and_run_locked(
            config_file=config_file, logs=logs, artifact_dir=run_dir, reports=reports,
            env=env, started=started, total_deadline=total_deadline,
        )
        finalize_started = time.monotonic()
        if finalize_started > total_deadline - FINALIZE_BUDGET_SECONDS:
            raise HighPreparationError("No reserved finalization time remained after the native run.")
        off = _read_arm_report(reports / ARM_REPORTS["off"])
        on = _read_arm_report(reports / ARM_REPORTS["on"])
        comparison = validate_comparison(off, on)
        comparison_status = str(comparison["comparison_status"])
        summary = {
            **comparison,
            "elapsed_seconds": round(time.monotonic() - started, 3),
            "compile_seconds": round(compile_seconds, 3),
            "compile_mode": "local_build_prepared_targets_locked_through_both_arms",
            "stable_products": identities,
            "runtime_limit_seconds": RUNTIME_BUDGET_SECONDS,
            "artifact_limit_bytes": ARTIFACT_BYTES,
        }
        status, exit_code = comparison_exit_status(comparison_status)
    except (HighPreparationError, OSError, ValueError) as error:
        summary = {
            "schema_version": 1,
            "status": "failed",
            "message": " ".join(str(error).split())[:512],
            "elapsed_seconds": round(time.monotonic() - started, 3),
            "compile_mode": "local_build_prepared_targets_locked_through_both_arms",
        }
    finally:
        if finalize_started is not None and time.monotonic() - finalize_started > FINALIZE_BUDGET_SECONDS:
            status = "failed"
            exit_code = 1
        summary["status"] = status
        summary["elapsed_seconds"] = round(time.monotonic() - started, 3)
        try:
            _atomic_write_json(run_dir / "summary.json", summary, SUMMARY_BYTES)
            artifact_usage(run_dir)
        except (HighPreparationError, OSError):
            status = "failed"
            exit_code = 1
    label = "INCONCLUSIVE" if status == "inconclusive" else status.upper()
    print(f"[media-high-preparation] {label}: {run_dir}", file=sys.stderr, flush=True)
    return exit_code


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true", help="Explicitly opt in to the muted native comparison.")
    parser.add_argument("--local-config", help="Use an existing public local signing-profile path.")
    parser.add_argument("--artifact-dir", help="Use this owned absolute artifact base; a fresh run directory is created below it.")
    args = parser.parse_args(argv)
    try:
        config_file = Path(args.local_config).expanduser() if args.local_config else None
        if config_file is not None and not config_file.is_absolute():
            raise HighPreparationError("--local-config must be an absolute path.")
        artifact_root = resolve_artifact_root(args.artifact_dir)
        return run(enabled=args.run, config_file=config_file, artifact_root=artifact_root)
    except HighPreparationError as error:
        print(f"[media-high-preparation] {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
