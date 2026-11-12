#!/usr/bin/env python3
"""Run a bounded, opt-in, locally signed single-player prewarm comparison."""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import os
import re
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent
NORMAL_RUNNER = Path(__file__).with_name("media-network-e2e.py")
TOTAL_BUDGET_SECONDS = 60 * 60
COMPILE_BUDGET_SECONDS = 25 * 60
RUNTIME_BUDGET_SECONDS = 30 * 60
FINALIZE_BUDGET_SECONDS = 60
CASE_BUDGET_SECONDS = 840
COMMAND_LOG_BYTES = 8 * 1024 * 1024
ARTIFACT_BYTES = 32 * 1024 * 1024
REPORT_BYTES = 64 * 1024
PREWARM_REPORT_BYTES = 256 * 1024
PREWARM_OBJECT_BYTES = 192 * 1024
PREWARM_BYTES = 8 * 1024 * 1024
PREWARM_TRANSFER_BYTES = 16 * 1024 * 1024
MAX_PREWARM_RANGES = 128
MAX_RESOURCE_ID_LENGTH = 128
U64_MAX = 2**64 - 1
STARTUP_PREFIX_BYTES = 512 * 1024
PREPARATION_SPAN_BYTES = 64 * 1024
MIN_PARENT_BYTES = 512 * 1024
MAX_PARENT_BYTES = 8 * 1024 * 1024
MAX_PARENT_COUNT = 8192
WINDOW_MAX_SECONDS = 22.0
TESTS = {
    "off": "media_prewarm_comparison_tests::native_single_player_prewarm_control",
    "on": "media_prewarm_comparison_tests::native_single_player_prewarm_treatment",
}
CASES = {
    "off": {"report": "prewarm-control.json", "scenario": "single_player_prewarm_control"},
    "on": {"report": "prewarm-treatment.json", "scenario": "single_player_prewarm_treatment"},
}


class PrewarmError(RuntimeError):
    pass


def _load_exact(name: str, path: Path) -> Any:
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise PrewarmError(f"Could not load required local helper: {path.name}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


normal = _load_exact("media_prewarm_normal_helpers", NORMAL_RUNNER)


def _prepared_signed_targets(local_build: Any, config: Path | None) -> Any:
    return local_build.prepared_targets({"probe", "media-tests"}, config_file=config)


def build_test_argv(binary: Path, mode: str) -> list[str]:
    if mode not in TESTS:
        raise PrewarmError("Unknown prewarm mode.")
    return [str(binary), TESTS[mode], "--exact", "--ignored", "--nocapture",
            "--test-threads=1", "--color=never"]


def _finite(value: Any) -> bool:
    return normal._numeric(value)


def _exact_int(value: Any, expected: int | None = None) -> bool:
    return normal._exact_int(value, expected)


def _work_deadline(configured: str | None, *, started: float) -> float:
    deadline = started + TOTAL_BUDGET_SECONDS
    if configured is not None:
        try:
            external = float(configured)
        except (TypeError, ValueError, OverflowError):
            raise PrewarmError("Workflow work deadline is invalid.") from None
        if not math.isfinite(external):
            raise PrewarmError("Workflow work deadline is invalid.")
        deadline = min(deadline, external)
    if deadline <= time.monotonic():
        raise PrewarmError("Workflow or runner work deadline has expired.")
    return deadline


def _read_json(path: Path) -> dict[str, Any]:
    if path.is_symlink() or not path.is_file():
        raise PrewarmError(f"Report is missing or non-regular: {path.name}")
    try:
        with path.open("rb") as report_file:
            raw = report_file.read(PREWARM_REPORT_BYTES + 1)
        if len(raw) > PREWARM_REPORT_BYTES:
            raise PrewarmError(f"Explicit prewarm report exceeds 256 KiB: {path.name}")
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=normal._unique_object,
                           parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite JSON")))
    except PrewarmError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError, RecursionError) as error:
        raise PrewarmError(f"Report JSON is invalid: {path.name}") from error
    if not isinstance(value, dict):
        raise PrewarmError(f"Report must be an object: {path.name}")
    prewarm = value.get("prewarm_comparison")
    if isinstance(prewarm, dict):
        try:
            encoded = json.dumps(prewarm, sort_keys=True, separators=(",", ":"),
                                 ensure_ascii=True, allow_nan=False).encode("utf-8")
        except (TypeError, ValueError, RecursionError) as error:
            raise PrewarmError(f"Prewarm comparison object cannot be serialized: {path.name}") from error
        if len(encoded) > PREWARM_OBJECT_BYTES:
            raise PrewarmError(f"Prewarm comparison object exceeds 192 KiB: {path.name}")
    return value


def _continuity(probe: dict[str, Any], report_name: str) -> dict[str, Any]:
    value = probe.get("decoded_frame_continuity")
    if not isinstance(value, dict) or not _exact_int(value.get("schema_version"), 1):
        raise PrewarmError(f"Decoded-frame continuity schema is missing: {report_name}")
    if (not _exact_int(value.get("decoded_frame_count")) or value["decoded_frame_count"] < 0
            or not _exact_int(value.get("observed_interval_count")) or value["observed_interval_count"] < 0
            or not _exact_int(value.get("large_gap_count")) or value["large_gap_count"] < 0
            or not _exact_int(value.get("observation_clock_anomaly_count"))
            or value["observation_clock_anomaly_count"] < 0
            or value.get("gap_threshold_seconds") != 0.5
            or value.get("coverage") != "between_observed_decoded_frames_only"
            or value.get("interpretation") != "decoded_frame_observation_gaps_not_visible_black_screen_acceptance"):
        raise PrewarmError(f"Decoded-frame continuity values are invalid: {report_name}")
    gap = value.get("maximum_interframe_gap_seconds")
    frame_count = value["decoded_frame_count"]
    interval_count = value["observed_interval_count"]
    if (interval_count > max(frame_count - 1, 0)
            or value["observation_clock_anomaly_count"] > frame_count
            or value["observation_clock_anomaly_count"] != 0
            or (interval_count == 0 and gap is not None)
            or (interval_count > 0 and (not _finite(gap) or gap < 0))
            or value["large_gap_count"] > interval_count):
        raise PrewarmError(f"Decoded-frame continuity gap summary is inconsistent: {report_name}")
    return value


def _diagnostic_window(probe: dict[str, Any], report_name: str) -> dict[str, Any]:
    value = probe.get("diagnostic_observation_window")
    if (not isinstance(value, dict) or not _exact_int(value.get("schema_version"), 1)
            or value.get("controls_excluded") is not True
            or value.get("requested_duration_seconds") != 600
            or not _finite(value.get("started_monotonic_seconds"))
            or not _finite(value.get("ended_monotonic_seconds"))):
        raise PrewarmError(f"Diagnostic observation window schema or clocks are invalid: {report_name}")
    start = value["started_monotonic_seconds"]
    end = value["ended_monotonic_seconds"]
    duration = end - start
    if start <= 0 or duration < 599.5 or duration > 605:
        raise PrewarmError(f"Diagnostic observation window duration is outside 599.5-605 seconds: {report_name}")
    continuity = _continuity({"decoded_frame_continuity": value.get("decoded_frame_continuity")}, report_name)
    global_continuity = _continuity(probe, report_name)
    if (not _exact_int(probe.get("decoded_frame_count"))
            or probe["decoded_frame_count"] != global_continuity["decoded_frame_count"]
            or continuity["decoded_frame_count"] > global_continuity["decoded_frame_count"]):
        raise PrewarmError(f"Global and window continuity frame counts are inconsistent: {report_name}")
    timeline = probe["playback_timeline"]["samples"]
    inferred_starts = [sample["observed_monotonic_seconds"] - sample["elapsed_seconds"] for sample in timeline]
    probe_start = inferred_starts[0]
    clock_tolerance = 1e-6
    if (probe_start <= 0 or any(abs(candidate - probe_start) > 2 for candidate in inferred_starts)
            or start < probe_start - clock_tolerance):
        raise PrewarmError(f"Observation window is not tied to the recorded initial-play clock: {report_name}")
    elapsed = probe.get("elapsed_seconds")
    if (not _finite(elapsed) or elapsed < 599.5 or elapsed > 720
            or end > probe_start + elapsed):
        raise PrewarmError(f"Observation window exceeds the observed probe deadline: {report_name}")
    return {"started_monotonic_seconds": start, "ended_monotonic_seconds": end,
            "duration_seconds": duration, "probe_started_monotonic_seconds": probe_start,
            "decoded_frame_continuity": continuity}


def _safe_resource_id(value: Any) -> bool:
    return (isinstance(value, str) and 1 <= len(value) <= MAX_RESOURCE_ID_LENGTH
            and value not in {".", ".."} and re.fullmatch(r"[A-Za-z0-9._-]+", value) is not None)


def _range_list(value: Any, resource_ids: set[str], name: str, report_name: str,
                *, totals: dict[str, int] | None = None,
                allow_empty: bool = False) -> list[dict[str, Any]]:
    minimum = 0 if allow_empty else 1
    if not isinstance(value, list) or not minimum <= len(value) <= MAX_PREWARM_RANGES:
        raise PrewarmError(f"{name} must contain {minimum}-128 canonical ranges: {report_name}")
    ranges: list[dict[str, Any]] = []
    previous: tuple[str, int, int] | None = None
    for item in value:
        if (not isinstance(item, dict) or set(item) != {"resource_id", "start", "end"}
                or not _safe_resource_id(item.get("resource_id"))
                or item["resource_id"] not in resource_ids
                or not _exact_int(item.get("start")) or not _exact_int(item.get("end"))
                or item["start"] < 0 or item["end"] > U64_MAX or item["end"] <= item["start"]
                or (totals is not None and item["end"] > totals[item["resource_id"]])):
            raise PrewarmError(f"{name} contains an invalid range: {report_name}")
        current = (item["resource_id"], item["start"], item["end"])
        if previous is not None and (
                current < previous or (current[0] == previous[0] and current[1] < previous[2])):
            raise PrewarmError(f"{name} is overlapping or not canonically ordered: {report_name}")
        previous = current
        ranges.append(item)
    return ranges


def _range_bytes(ranges: list[dict[str, Any]], name: str, report_name: str) -> int:
    total = 0
    for item in ranges:
        total += item["end"] - item["start"]
        if total > U64_MAX:
            raise PrewarmError(f"{name} byte sum overflows u64: {report_name}")
    return total


def _normalized_union(ranges: list[dict[str, Any]]) -> list[tuple[str, int, int]]:
    normalized: list[tuple[str, int, int]] = []
    spans = sorted((item["resource_id"], item["start"], item["end"]) for item in ranges)
    for resource_id, start, end in spans:
        if normalized and normalized[-1][0] == resource_id and start <= normalized[-1][2]:
            previous_id, previous_start, previous_end = normalized[-1]
            normalized[-1] = (previous_id, previous_start, max(previous_end, end))
        else:
            normalized.append((resource_id, start, end))
    return normalized


def _range_is_covered(wanted: tuple[str, int, int], ranges: list[dict[str, Any]]) -> bool:
    resource_id, start, end = wanted
    cursor = start
    for item in ranges:
        if item["resource_id"] != resource_id or item["end"] <= cursor:
            continue
        if item["start"] > cursor:
            return False
        cursor = max(cursor, item["end"])
        if cursor >= end:
            return True
    return cursor >= end


def _parent_span(total: int, offset: int) -> tuple[int, int] | None:
    startup_end = min(total, STARTUP_PREFIX_BYTES)
    if offset < startup_end:
        return 0, startup_end
    if offset >= total:
        return None
    remaining = total - startup_end
    parent_count = max(MAX_PARENT_COUNT - 1, 1)
    parent_size = min(MAX_PARENT_BYTES, max(MIN_PARENT_BYTES,
                         (remaining + parent_count - 1) // parent_count))
    parent_index = (offset - startup_end) // parent_size
    parent_start = startup_end + parent_index * parent_size
    return parent_start, min(total, parent_start + parent_size)


def _expected_transfer_plan(selected: list[dict[str, Any]], totals: dict[str, int],
                            report_name: str) -> list[dict[str, Any]]:
    planned: set[tuple[str, int, int]] = set()
    selected_ids = {item["resource_id"] for item in selected}
    for resource_id in selected_ids:
        planned.add((resource_id, 0, min(totals[resource_id], STARTUP_PREFIX_BYTES)))
    for item in selected:
        resource_id, total = item["resource_id"], totals[item["resource_id"]]
        cursor = max(item["start"], min(total, STARTUP_PREFIX_BYTES))
        while cursor < item["end"]:
            parent = _parent_span(total, cursor)
            if parent is None or parent[1] <= cursor:
                raise PrewarmError(f"Selected range cannot be mapped to a preparation span: {report_name}")
            parent_start, parent_end = parent
            chunk_start = parent_start
            while chunk_start < parent_end:
                chunk_end = min(parent_end, chunk_start + PREPARATION_SPAN_BYTES)
                if chunk_start < item["end"] and cursor < chunk_end:
                    planned.add((resource_id, chunk_start, chunk_end))
                chunk_start = chunk_end
            cursor = parent_end
    expected = [{"resource_id": resource_id, "start": start, "end": end}
                for resource_id, start, end in sorted(planned)]
    if (not expected or len(expected) > MAX_PREWARM_RANGES
            or _range_bytes(expected, "transfer_plan", report_name) > PREWARM_TRANSFER_BYTES):
        raise PrewarmError(f"Expected preparation plan exceeds its named bounds: {report_name}")
    return expected


PREWARM_KEYS = {
    "schema_version", "mode", "boundary_policy", "recovery_snapshot",
    "target_window_start_seconds", "target_window_end_seconds", "byte_budget",
    "planned_bytes", "transfer_byte_budget", "planned_transfer_bytes", "attempt_count",
    "attempted_span_bytes", "attempted_ranges", "cache_published_bytes",
    "cache_published_spans", "preempted_operation_count", "foreground_defer_count",
    "elapsed_seconds", "coverage_semantics", "cache_covered_bytes_before",
    "cache_covered_spans_before", "cache_covered_bytes_after",
    "cache_covered_spans_after", "selected_ranges", "transfer_plan", "resource_ids",
    "resource_total_lengths", "completed", "failure", "provenance",
}
ATTEMPT_KEYS = {
    "resource_id", "resource_total_length", "start", "end", "purpose", "status_code",
    "headers_validated", "body_validated", "identity_validated", "cache_published", "outcome",
}
ATTEMPT_PURPOSES = {"cache_fill", "resource_discovery", "identity_probe"}
ATTEMPT_OUTCOMES = {
    "in_flight", "headers_received", "headers_validated", "body_validated", "cache_published",
    "preempted", "cancelled", "session_removing", "identity_changed", "quota_exceeded",
    "upstream_status", "network", "io", "range_unsupported", "invalid_response",
    "deadline_exceeded", "interrupted",
}
FAILURE_KEYS = {"category", "resource_id", "start", "end", "upstream_status"}
PROVENANCE = {
    "shared_origin_counter_scope": "playback_and_preparation",
    "attempt_bytes_semantics": "sum_of_requested_range_lengths_not_wire_bytes",
    "identity_probe_attempts_included": True,
    "transfer_plan_semantics": "empty_cache_upper_bound_startup_prefix_and_64k_playback_preparation_spans",
    "expanded_transfer_may_overlap_buffered_media": True,
}


def _validate_failure_state(failure: Any, attempted: list[dict[str, Any]],
                           resource_ids: set[str], *, completed: bool,
                           mode: str, report_name: str) -> None:
    if not isinstance(failure, dict) or set(failure) != FAILURE_KEYS:
        raise PrewarmError(f"Prewarm root failure fields are invalid: {report_name}")
    category = failure["category"]
    resource_id = failure["resource_id"]
    failure_start, failure_end = failure["start"], failure["end"]
    upstream_status = failure["upstream_status"]
    if (category is not None and (not isinstance(category, str)
                                  or category not in ATTEMPT_OUTCOMES)
            or resource_id is not None and (not _safe_resource_id(resource_id)
                                            or resource_id not in resource_ids)
            or failure_start is not None and (not _exact_int(failure_start) or failure_start < 0)
            or failure_end is not None and (not _exact_int(failure_end) or failure_start is None
                                            or failure_end <= failure_start or failure_end > U64_MAX)
            or upstream_status is not None and (not _exact_int(upstream_status)
                                                or not 100 <= upstream_status <= 599)):
        raise PrewarmError(f"Prewarm root failure values are invalid: {report_name}")
    if completed:
        if any(failure[key] is not None for key in FAILURE_KEYS):
            raise PrewarmError(f"Completed prewarm report contains root failure details: {report_name}")
        return
    matches = [attempt for attempt in attempted if attempt["outcome"] == category
               and (resource_id is None or attempt["resource_id"] == resource_id)
               and (failure_start is None or attempt["start"] == failure_start)
               and (failure_end is None or attempt["end"] == failure_end)
               and (upstream_status is None or attempt["status_code"] == upstream_status)]
    if mode == "off" or category is None or not matches:
        raise PrewarmError(f"Failed prewarm root cause does not match its attempt ledger: {report_name}")


def _prewarm_diagnostic_data(report: dict[str, Any], mode: str,
                             report_name: str) -> dict[str, Any]:
    value = report.get("prewarm_comparison")
    expected_mode = "off" if mode == "off" else "on"
    if (not isinstance(value, dict) or set(value) != PREWARM_KEYS
            or not _exact_int(value.get("schema_version"), 4)
            or value.get("mode") != expected_mode
            or value.get("boundary_policy") != "unbuffered_after_20s_safety"):
        raise PrewarmError(f"Prewarm comparison identity or exact v4 keys are invalid: {report_name}")
    snapshot = value.get("recovery_snapshot")
    if (not isinstance(snapshot, dict) or not _exact_int(snapshot.get("schema_version"), 1)
            or any(not _finite(snapshot.get(key)) for key in
                   ("observed_monotonic_seconds", "playhead_seconds", "loaded_end_seconds"))
            or snapshot["observed_monotonic_seconds"] <= 0 or snapshot["playhead_seconds"] < 0
            or snapshot["playhead_seconds"] > 3600
            or snapshot["loaded_end_seconds"] < snapshot["playhead_seconds"]
            or snapshot["loaded_end_seconds"] > 3600):
        raise PrewarmError(f"Recovery snapshot is invalid: {report_name}")
    start, end = value.get("target_window_start_seconds"), value.get("target_window_end_seconds")
    safe_start = max(snapshot["playhead_seconds"] + 20.0, snapshot["loaded_end_seconds"])
    if (not _finite(start) or not _finite(end) or start < safe_start or end <= start or end > 3600
            or end - start > WINDOW_MAX_SECONDS):
        raise PrewarmError(f"Target window violates the unbuffered 20-second safety boundary: {report_name}")

    id_map = value.get("resource_ids")
    lengths = value.get("resource_total_lengths")
    if (not isinstance(id_map, dict) or set(id_map) != {"video", "audio"}
            or any(not _safe_resource_id(item) for item in id_map.values())
            or id_map["video"] == id_map["audio"]
            or not isinstance(lengths, dict) or set(lengths) != {"video", "audio"}
            or any(not _exact_int(lengths.get(kind)) or not 1 <= lengths[kind] <= U64_MAX
                   for kind in ("video", "audio"))):
        raise PrewarmError(f"Prewarm resource identities or total lengths are invalid: {report_name}")
    resource_ids = set(id_map.values())
    totals = {id_map[kind]: lengths[kind] for kind in ("video", "audio")}

    selected = _range_list(value.get("selected_ranges"), resource_ids, "selected_ranges", report_name,
                           totals=totals)
    selected_bytes = _range_bytes(selected, "selected_ranges", report_name)
    budget, planned = value.get("byte_budget"), value.get("planned_bytes")
    if (not _exact_int(budget, PREWARM_BYTES) or not _exact_int(planned)
            or planned <= 0 or planned > PREWARM_BYTES or planned != selected_bytes):
        raise PrewarmError(f"Selected logical byte budget is invalid: {report_name}")

    transfer = _range_list(value.get("transfer_plan"), resource_ids, "transfer_plan", report_name,
                           totals=totals)
    transfer_bytes = _range_bytes(transfer, "transfer_plan", report_name)
    expected_plan = _expected_transfer_plan(selected, totals, report_name)
    if (not _exact_int(value.get("transfer_byte_budget"), PREWARM_TRANSFER_BYTES)
            or not _exact_int(value.get("planned_transfer_bytes"))
            or value["planned_transfer_bytes"] <= 0
            or value["planned_transfer_bytes"] > PREWARM_TRANSFER_BYTES
            or value["planned_transfer_bytes"] != transfer_bytes
            or transfer != expected_plan):
        raise PrewarmError(f"Transfer plan does not match the empty-cache preparation policy: {report_name}")

    if value.get("coverage_semantics") != "sum_of_fully_readable_selected_ranges":
        raise PrewarmError(f"Selected-range coverage semantics are invalid: {report_name}")
    selected_set = {(item["resource_id"], item["start"], item["end"]) for item in selected}
    for count_key, spans_key in (("cache_covered_bytes_before", "cache_covered_spans_before"),
                                 ("cache_covered_bytes_after", "cache_covered_spans_after")):
        count, spans = value.get(count_key), value.get(spans_key)
        if count is None and spans is None:
            continue
        if count is None or spans is None:
            raise PrewarmError(f"{count_key} and {spans_key} must be null together: {report_name}")
        coverage = _range_list(spans, resource_ids, spans_key, report_name,
                               totals=totals, allow_empty=True)
        if (not _exact_int(count) or count < 0 or count > planned
                or _range_bytes(coverage, spans_key, report_name) != count
                or any((item["resource_id"], item["start"], item["end"]) not in selected_set
                       for item in coverage)):
            raise PrewarmError(f"{count_key} does not match exact readable selected ranges: {report_name}")

    attempted = value.get("attempted_ranges")
    attempt_count = value.get("attempt_count")
    attempted_bytes = value.get("attempted_span_bytes")
    if (not isinstance(attempted, list) or len(attempted) > MAX_PREWARM_RANGES
            or not _exact_int(attempt_count) or attempt_count != len(attempted)
            or not _exact_int(attempted_bytes) or attempted_bytes < 0
            or attempted_bytes > PREWARM_TRANSFER_BYTES):
        raise PrewarmError(f"Attempt counters exceed bounds or disagree with the ledger: {report_name}")
    measured_attempted_bytes = 0
    published_attempts: list[dict[str, Any]] = []
    for attempt in attempted:
        if not isinstance(attempt, dict) or set(attempt) != ATTEMPT_KEYS:
            raise PrewarmError(f"Attempt ledger entry has unexpected or missing fields: {report_name}")
        resource_id = attempt.get("resource_id")
        purpose = attempt.get("purpose")
        total_length = attempt.get("resource_total_length")
        status_code = attempt.get("status_code")
        headers = attempt.get("headers_validated")
        body = attempt.get("body_validated")
        identity = attempt.get("identity_validated")
        cache_published = attempt.get("cache_published")
        outcome = attempt.get("outcome")
        if (not _safe_resource_id(resource_id) or resource_id not in resource_ids
                or purpose not in ATTEMPT_PURPOSES or outcome not in ATTEMPT_OUTCOMES
                or not _exact_int(attempt.get("start")) or not _exact_int(attempt.get("end"))
                or attempt["start"] < 0 or attempt["end"] <= attempt["start"]
                or attempt["end"] > U64_MAX
                or (total_length is not None and
                    (not _exact_int(total_length) or not 1 <= total_length <= U64_MAX))
                or (purpose != "resource_discovery" and total_length != totals[resource_id])
                or (purpose == "resource_discovery" and total_length not in (None, totals[resource_id]))
                or (total_length is not None and attempt["end"] > total_length)
                or (status_code is not None and
                    (not _exact_int(status_code) or not 100 <= status_code <= 599))
                or any(flag is not None and type(flag) is not bool
                       for flag in (headers, body, identity, cache_published))):
            raise PrewarmError(f"Attempt ledger entry has invalid identity, range, or scalar fields: {report_name}")
        measured_attempted_bytes += attempt["end"] - attempt["start"]
        if measured_attempted_bytes > PREWARM_TRANSFER_BYTES:
            raise PrewarmError(f"Attempted span-byte total exceeds 16 MiB: {report_name}")
        if (status_code is None and headers is not None
                or status_code is not None and headers is False and body is not None
                or body is not None and headers is not True
                or headers is True and (status_code is None or total_length is None)
                or purpose != "cache_fill" and cache_published is not None
                or cache_published is True and (
                    headers is not True or body is not True or outcome != "cache_published")):
            raise PrewarmError(f"Attempt validation and publication flags are inconsistent: {report_name}")
        if outcome == "headers_received" and (status_code is None or headers is not None or body is not None):
            raise PrewarmError(f"headers_received outcome has impossible validation flags: {report_name}")
        if outcome == "headers_validated" and (headers is not True or body is not None):
            raise PrewarmError(f"headers_validated outcome has impossible validation flags: {report_name}")
        if outcome == "body_validated" and (headers is not True or body is not True):
            raise PrewarmError(f"body_validated outcome lacks validated headers and body: {report_name}")
        if outcome == "cache_published" and (
                purpose != "cache_fill" or headers is not True or body is not True
                or cache_published is not True):
            raise PrewarmError(f"cache_published outcome lacks a confirmed durable publication: {report_name}")
        if outcome == "identity_changed" and identity is not False:
            raise PrewarmError(f"identity_changed outcome lacks a failed identity validation: {report_name}")
        if outcome == "upstream_status" and (status_code is None or headers is not False):
            raise PrewarmError(f"upstream_status outcome lacks a rejected HTTP status: {report_name}")
        span = {"resource_id": resource_id, "start": attempt["start"], "end": attempt["end"]}
        if purpose == "cache_fill" and not _range_is_covered(
                (resource_id, attempt["start"], attempt["end"]), transfer):
            raise PrewarmError(f"Cache-fill attempt is outside the declared transfer-plan union: {report_name}")
        if cache_published is True:
            published_attempts.append(span)
    if measured_attempted_bytes != attempted_bytes:
        raise PrewarmError(f"Attempted span-byte counter does not match the send ledger: {report_name}")

    published = _range_list(value.get("cache_published_spans"), resource_ids,
                             "cache_published_spans", report_name, totals=totals, allow_empty=True)
    published_bytes = value.get("cache_published_bytes")
    expected_published_union = _normalized_union(published_attempts)
    observed_published_union = _normalized_union(published)
    if (not _exact_int(published_bytes) or published_bytes < 0
            or published_bytes != _range_bytes(published, "cache_published_spans", report_name)
            or observed_published_union != expected_published_union):
        raise PrewarmError(f"Published-span union or byte count does not match confirmed cache fills: {report_name}")

    preempted_count = value.get("preempted_operation_count")
    defer_count = value.get("foreground_defer_count")
    elapsed = value.get("elapsed_seconds")
    if (not _exact_int(preempted_count) or preempted_count < 0
            or not _exact_int(defer_count) or defer_count < 0
            or not _finite(elapsed) or elapsed < 0 or elapsed > 15
            or type(value.get("completed")) is not bool):
        raise PrewarmError(f"Prewarm completion flag, retry counters, or elapsed bound is invalid: {report_name}")
    failure = value.get("failure")
    _validate_failure_state(failure, attempted, resource_ids, completed=value["completed"],
                            mode=mode, report_name=report_name)
    if value.get("provenance") != PROVENANCE:
        raise PrewarmError(f"Prewarm provenance is missing, changed, or has unknown fields: {report_name}")

    if mode == "off":
        if (attempt_count != 0 or attempted_bytes != 0 or published_bytes != 0 or published
                or preempted_count != 0 or defer_count != 0):
            raise PrewarmError("Control report claims prewarm sends, publication, or admission activity.")
    return value


def _prewarm_contract(report: dict[str, Any], mode: str, report_name: str) -> dict[str, Any]:
    value = _prewarm_diagnostic_data(report, mode, report_name)
    if value["completed"] is not True:
        raise PrewarmError(f"Prewarm preparation is not a completed case: {report_name}")
    if mode == "on":
        selected = {(item["resource_id"], item["start"], item["end"])
                    for item in value["selected_ranges"]}
        after = value["cache_covered_spans_after"]
        if (after is None or
                {(item["resource_id"], item["start"], item["end"]) for item in after} != selected):
            raise PrewarmError("Treatment did not prove actual readable coverage of every selected range.")
    return value


def validate_report(path: Path, mode: str) -> dict[str, Any]:
    if mode not in CASES:
        raise PrewarmError("Unknown prewarm mode contract.")
    report = _read_json(path)
    case = TESTS[mode]
    contract = CASES[mode]
    if (not _exact_int(report.get("schema_version"), 1) or report.get("status") != "passed"
            or report.get("diagnostic_only") is not True or report.get("case_full_name") != case
            or report.get("scenario") != contract["scenario"]
            or report.get("source_duration_seconds") != 3600):
        raise PrewarmError(f"Prewarm report identity/status is invalid: {path.name}")
    probe = report.get("probe")
    if (not isinstance(probe, dict) or not _exact_int(probe.get("schema_version"), 1)
            or probe.get("status") != "completed"
            or probe.get("diagnostic_optional_recovery_high") is not True):
        raise PrewarmError(f"Probe report is not completed: {path.name}")
    if (probe.get("requested_duration_seconds") != 600 or probe.get("overall_deadline_seconds") != 720
            or probe.get("deadline_mode") != "cooperative" or probe.get("audio_track_present") is not True
            or not _exact_int(probe.get("enabled_audio_track_count")) or probe["enabled_audio_track_count"] < 1
            or not _exact_int(probe.get("decoded_frame_count")) or probe["decoded_frame_count"] <= 0):
        raise PrewarmError(f"Probe acceptance health is incomplete: {path.name}")
    normal._validate_playback_timeline(probe, 3600.0, path.name)
    observation_window = _diagnostic_window(probe, path.name)
    window_start = observation_window["started_monotonic_seconds"]
    window_end = observation_window["ended_monotonic_seconds"]
    steps = probe.get("steps")
    required = {"play", "pause", "resume", "seek_forward", "seek_forward_play", "seek_back",
                "seek_back_play", "rate_1_25", "duration_remainder"}
    if not isinstance(steps, list) or any(not isinstance(step, dict) for step in steps):
        raise PrewarmError(f"Playback control steps are incomplete: {path.name}")
    named_steps = {name: [step for step in steps if step.get("name") == name]
                   for name in required}
    if any(len(named_steps[name]) != 1 or named_steps[name][0].get("status") != "completed"
           for name in required):
        raise PrewarmError(f"Playback control steps are incomplete or claim a skipped control: {path.name}")
    play_step = named_steps["play"][0]
    if (not _finite(play_step.get("elapsed_seconds")) or play_step["elapsed_seconds"] < 599.5):
        raise PrewarmError(f"Initial uninterrupted play did not cover the observation window: {path.name}")
    expectations = probe.get("video_size_expectations")
    sizes = ((640, 360), (320, 180), (640, 360))
    if (not isinstance(expectations, dict) or expectations.get("enabled") is not True
            or expectations.get("semantics") != "ordered_decoded_size_subsequence"
            or not _exact_int(expectations.get("minimum_distinct_display_timestamps_per_size"), 3)
            or not isinstance(expectations.get("entries"), list) or len(expectations["entries"]) != 3):
        raise PrewarmError(f"High-low-high expectation schema is invalid: {path.name}")
    recovery_samples: list[dict[str, Any]] | None = None
    phases: list[list[float]] = []
    for index, (entry, size) in enumerate(zip(expectations["entries"], sizes, strict=True)):
        if not isinstance(entry, dict) or (entry.get("expected_width"), entry.get("expected_height")) != size:
            raise PrewarmError(f"Video-size order is invalid: {path.name}")
        samples = entry.get("samples")
        valid_samples = _valid_samples(samples)
        if index < 2:
            if entry.get("matched") is not True or not _exact_int(entry.get("distinct_frame_count")) \
                    or entry["distinct_frame_count"] < 3 or len(valid_samples) != 3:
                raise PrewarmError(f"Required baseline/bad-high decoded samples are incomplete: {path.name}")
            phases.append([s["observed_monotonic_seconds"] for s in valid_samples])
        else:
            optional = entry.get("diagnostic_optional_recovery_high") is True
            if not optional:
                raise PrewarmError(f"Optional recovery entry flag is missing: {path.name}")
            if entry.get("matched") is not True:
                if not optional or valid_samples:
                    raise PrewarmError(f"Missing recovery high is not explicitly censored: {path.name}")
                if expectations.get("complete") is not False:
                    raise PrewarmError(f"Missing recovery high falsely claims complete expectations: {path.name}")
                recovery_samples = None
            else:
                if optional and len(valid_samples) != 3:
                    raise PrewarmError(f"Optional recovered-high samples must be empty or a valid triple: {path.name}")
                if not _exact_int(entry.get("distinct_frame_count")) or entry["distinct_frame_count"] < 3 \
                        or len(valid_samples) != 3 or expectations.get("complete") is not True:
                    raise PrewarmError(f"Recovered-high evidence is incomplete: {path.name}")
                recovery_samples = valid_samples
    if len(phases) != 2:
        raise PrewarmError("Required decoded phases are absent.")
    phase_samples = [sample for entry in expectations["entries"][:2]
                     for sample in _valid_samples(entry.get("samples"))]
    if any(not window_start <= sample["observed_monotonic_seconds"] <= window_end
           for sample in phase_samples):
        raise PrewarmError(f"Timed decoded-size phase samples escape the observation window: {path.name}")
    if recovery_samples and any(not window_start <= sample["observed_monotonic_seconds"] <= window_end
                                for sample in recovery_samples):
        raise PrewarmError(f"Recovered-high samples escape the observation window: {path.name}")
    requests = report.get("requests")
    if not isinstance(requests, dict):
        raise PrewarmError("Playback origin request evidence is absent.")
    normal._validate_progressive_origin_trace(report, "diagnostic_prewarm", path.name)
    fault = report.get("fault_timeline")
    if not isinstance(fault, dict) or fault.get("enabled") is not True or fault.get("bad_high_status_code") != 503:
        raise PrewarmError("Fault timeline is missing its bounded high-fault case.")
    bounds = [fault.get(key) for key in ("decoder_ready_monotonic_seconds", "baseline_start_monotonic_seconds",
              "bad_high_before_monotonic_seconds", "bad_high_after_monotonic_seconds",
              "recovery_before_monotonic_seconds", "recovery_after_monotonic_seconds")]
    if not all(_finite(item) and item > 0 for item in bounds):
        raise PrewarmError("Fault timeline clock bounds are incomplete.")
    decoder, baseline, bad_before, bad_after, recovery_before, recovery_after = map(float, bounds)
    if not (decoder <= baseline and baseline + 6 <= bad_before <= bad_after <= recovery_before <= recovery_after):
        raise PrewarmError("Fault timeline clock bounds are inconsistent.")
    if any(t >= bad_before for t in phases[0]) or any(t < bad_after or t >= recovery_before for t in phases[1]):
        raise PrewarmError("Decoded samples do not align with baseline and bad-high phases.")
    if recovery_samples and any(s["observed_monotonic_seconds"] < recovery_after for s in recovery_samples):
        raise PrewarmError("Recovered-high samples precede recovery phase.")
    ingress = report.get("lan_ingress")
    if not isinstance(ingress, dict):
        raise PrewarmError("LAN client ingress evidence is absent.")
    # Representation 0 is high; phase 2 is recovery. This is the client path, not origin prewarm traffic.
    matrix = ingress.get("count_by_phase_and_representation")
    if not isinstance(matrix, list) or len(matrix) != 3 or any(not isinstance(row, list) or len(row) != 3 for row in matrix):
        raise PrewarmError("LAN ingress matrix dimensions are invalid.")
    if any(not _exact_int(n) or n < 0 for row in matrix for n in row):
        raise PrewarmError("LAN ingress matrix has invalid counts.")
    client_high = matrix[2][0] > 0
    if recovery_samples and not client_high:
        raise PrewarmError("Decoded recovery high is missing corresponding client high ingress evidence.")
    if not recovery_samples and client_high:
        # Request evidence can precede decoding; retain it as censored decode, never a zero latency.
        pass
    recovery_observation = probe.get("recovery_observation")
    normal._observation_state(recovery_observation, expected_at_or_after=recovery_after,
                              asset_duration=3600, must_reach_asset_end=False)
    if not window_start <= recovery_observation["observed_monotonic_seconds"] <= window_end:
        raise PrewarmError(f"Recovery observation escapes the diagnostic window: {path.name}")
    prewarm = _prewarm_contract(report, mode, path.name)
    if prewarm["recovery_snapshot"]["observed_monotonic_seconds"] < recovery_after:
        raise PrewarmError(f"Prewarm recovery snapshot precedes the recovery bracket: {path.name}")
    if not window_start <= prewarm["recovery_snapshot"]["observed_monotonic_seconds"] <= window_end:
        raise PrewarmError(f"Prewarm recovery snapshot escapes the diagnostic window: {path.name}")
    continuity = observation_window["decoded_frame_continuity"]
    if continuity["observed_interval_count"] > max(0, continuity["decoded_frame_count"] - 1):
        raise PrewarmError(f"Window continuity counters are inconsistent: {path.name}")
    observation_window["recovery_high_censored_after_window"] = recovery_samples is None
    return {"mode": mode, "report": report, "probe": probe, "recovery_samples": recovery_samples,
            "client_high_ingress": matrix[2][0], "prewarm": prewarm, "continuity": continuity,
            "observation_window": observation_window}


def _valid_samples(value: Any) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        return []
    if len(value) not in (0, 3):
        raise PrewarmError("Decoded expectation sample set must be empty or exactly three samples.")
    result = []
    for sample in value:
        if not isinstance(sample, dict) or any(not _finite(sample.get(key)) for key in
                ("display_time_seconds", "observed_elapsed_seconds", "observed_monotonic_seconds")):
            raise PrewarmError("Decoded-size timing sample is malformed.")
        if sample["display_time_seconds"] < 0 or sample["observed_elapsed_seconds"] < 0 \
                or sample["observed_elapsed_seconds"] > 720 or sample["observed_monotonic_seconds"] <= 0:
            raise PrewarmError("Decoded-size timing sample is out of bounds.")
        result.append(sample)
    if len({s["display_time_seconds"] for s in result}) != len(result):
        raise PrewarmError("Decoded-size samples lack distinct display timestamps.")
    if len({s["observed_monotonic_seconds"] for s in result}) != len(result):
        raise PrewarmError("Decoded-size samples lack distinct monotonic timestamps.")
    if [s["observed_monotonic_seconds"] for s in result] != sorted(s["observed_monotonic_seconds"] for s in result):
        raise PrewarmError("Decoded-size sample monotonic clocks are out of order.")
    return result


def _comparison(off: dict[str, Any], on: dict[str, Any]) -> dict[str, Any]:
    recovered = off["recovery_samples"] is not None and on["recovery_samples"] is not None
    def latency(case: dict[str, Any]) -> dict[str, Any] | None:
        samples = case["recovery_samples"]
        if not samples:
            return None
        first = samples[0]["observed_monotonic_seconds"]
        fault = case["report"]["fault_timeline"]
        return {"first_decoded_monotonic_seconds": first,
                "lower_bound_seconds": first - fault["recovery_after_monotonic_seconds"],
                "upper_bound_seconds": first - fault["recovery_before_monotonic_seconds"]}
    off_latency, on_latency = latency(off), latency(on)
    origin_counts = []
    for case in (off, on):
        req = case["report"]["requests"]
        origin_counts.append({"count_by_phase_and_representation": req["count_by_phase_and_representation"],
                              "successful_by_representation": req["successful_by_representation"],
                              "high_fault_status_count": req["high_fault_status_count"]})
    gaps = [case["continuity"]["maximum_interframe_gap_seconds"] for case in (off, on)]
    waiting_samples = []
    for case in (off, on):
        samples = case["probe"]["playback_timeline"]["samples"]
        waiting_samples.append(sum(sample["time_control_status"] == "waiting" for sample in samples))
    latency_delta = None
    latency_delta_bounds = None
    if recovered:
        latency_delta = off_latency["lower_bound_seconds"] - on_latency["lower_bound_seconds"]
        latency_delta_bounds = {
            "lower_bound_seconds": off_latency["lower_bound_seconds"] - on_latency["upper_bound_seconds"],
            "upper_bound_seconds": off_latency["upper_bound_seconds"] - on_latency["lower_bound_seconds"],
        }
    return {
        "schema_version": 2,
        "prewarm_report_schema_version": 4,
        "diagnostic_only": True,
        "status": "valid_comparison" if recovered else "negative_diagnostic",
        "interpretation": "raw observed latency comparison only; no causal proof or optimization gate",
        "matched_profile": {"source_duration_seconds": 3600, "probe_duration_seconds": 600,
                            "probe_deadline_seconds": 720, "single_player_per_invocation": True},
        "observation_windows": {"control": off["observation_window"], "treatment": on["observation_window"],
                                "recovery_high_censored_after_window": {
                                    "control": off["observation_window"]["recovery_high_censored_after_window"],
                                    "treatment": on["observation_window"]["recovery_high_censored_after_window"]}},
        "target_windows": {"control": {"start_seconds": off["prewarm"]["target_window_start_seconds"],
                                       "end_seconds": off["prewarm"]["target_window_end_seconds"]},
                           "treatment": {"start_seconds": on["prewarm"]["target_window_start_seconds"],
                                         "end_seconds": on["prewarm"]["target_window_end_seconds"]},
                           "may_differ_due_to_actual_buffer_state": True},
        "recovery_high": {"control_latency_bounds": off_latency,
                          "treatment_latency_bounds": on_latency,
                          "raw_observed_latency_delta_seconds": latency_delta,
                          "latency_delta_bounds_seconds": latency_delta_bounds,
                          "censored_when_not_decoded": True},
        "client_high_request_count": {"control": off["client_high_ingress"], "treatment": on["client_high_ingress"],
                                      "source": "lan_ingress_client_requests_not_origin_prewarm"},
        "origin_request_summary": origin_counts,
        "decoded_continuity": {"maximum_interframe_gap_seconds": gaps,
                                "decoded_frame_count": [case["continuity"]["decoded_frame_count"] for case in (off, on)],
                                "observed_interval_count": [case["continuity"]["observed_interval_count"] for case in (off, on)],
                                "large_gap_count": [case["continuity"]["large_gap_count"] for case in (off, on)],
                                "clock_anomaly_count": [case["continuity"]["observation_clock_anomaly_count"] for case in (off, on)],
                                "timeline_waiting_sample_count": waiting_samples},
        "prewarm_readiness": {
            "coverage_semantics": "sum_of_fully_readable_selected_ranges",
            "attempt_bytes_semantics": "sum_of_requested_range_lengths_not_wire_bytes",
            "transfer_plan_semantics": "empty_cache_upper_bound_startup_prefix_and_64k_playback_preparation_spans",
            "expanded_transfer_may_overlap_buffered_media": True,
            "control": {"planned_logical_selected_bytes": off["prewarm"]["planned_bytes"],
                        "planned_expanded_transfer_bytes": off["prewarm"]["planned_transfer_bytes"],
                        "actual_attempt_count": off["prewarm"]["attempt_count"],
                        "actual_attempted_span_bytes": off["prewarm"]["attempted_span_bytes"],
                        "preempted_operation_count": off["prewarm"]["preempted_operation_count"],
                        "foreground_defer_count": off["prewarm"]["foreground_defer_count"],
                        "cache_published_span_count": len(off["prewarm"]["cache_published_spans"]),
                        "cache_published_bytes": off["prewarm"]["cache_published_bytes"],
                        "covered_logical_selected_bytes": off["prewarm"]["cache_covered_bytes_after"]},
            "treatment": {"planned_logical_selected_bytes": on["prewarm"]["planned_bytes"],
                          "planned_expanded_transfer_bytes": on["prewarm"]["planned_transfer_bytes"],
                          "actual_attempt_count": on["prewarm"]["attempt_count"],
                          "actual_attempted_span_bytes": on["prewarm"]["attempted_span_bytes"],
                          "preempted_operation_count": on["prewarm"]["preempted_operation_count"],
                          "foreground_defer_count": on["prewarm"]["foreground_defer_count"],
                          "cache_published_span_count": len(on["prewarm"]["cache_published_spans"]),
                          "cache_published_bytes": on["prewarm"]["cache_published_bytes"],
                          "covered_logical_selected_bytes": on["prewarm"]["cache_covered_bytes_after"]}},
        "acceptance_passed": False,
    }


def validate_single_case_log(path: Path, case_name: str, *, returncode: int = 0,
                             timed_out: bool = False) -> None:
    if timed_out:
        raise PrewarmError("Single-case Rust test process timed out.")
    if returncode != 0:
        raise PrewarmError(f"Single-case Rust test process exited {returncode}.")
    try:
        with path.open("rb") as log:
            raw = log.read(COMMAND_LOG_BYTES + 1)
        if len(raw) > COMMAND_LOG_BYTES:
            raise PrewarmError("Single-case Rust test log exceeds its output bound.")
        lines = raw.decode("utf-8").splitlines()
    except PrewarmError:
        raise
    except (OSError, UnicodeError) as error:
        raise PrewarmError("Single-case Rust test log is missing or invalid.") from error
    seen = 0
    pending = False
    summaries: list[int] = []
    summary = re.compile(
        r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
        r"\d+ filtered out(?:; finished in \d+(?:\.\d+)?s)?")
    for index, line in enumerate(lines):
        if line.startswith("test result:"):
            if pending or summary.fullmatch(line) is None:
                raise PrewarmError("Single-case Rust log contains a malformed or premature summary.")
            summaries.append(index)
            continue
        if line.startswith("test "):
            match = re.fullmatch(r"test (.+) \.\.\.(?: (.*))?", line)
            if pending or match is None or match.group(1) != case_name or seen:
                raise PrewarmError("Single-case Rust log has a duplicate, unexpected, or malformed case.")
            seen += 1
            status = match.group(2)
            if status is None:
                pending = True
            elif status == "ok":
                pass
            elif status in {"FAILED", "ignored", "bench"}:
                raise PrewarmError("Single-case Rust test did not pass.")
            elif status.startswith(("ok ", "FAILED ", "ignored ", "bench ")):
                raise PrewarmError("Single-case Rust test has an ambiguous inline status.")
            else:
                pending = True
            continue
        if line in {"ok", "FAILED", "ignored", "bench"}:
            if line != "ok" or not pending:
                raise PrewarmError("Single-case Rust log contains an orphan or non-passing status.")
            pending = False
    if (pending or seen != 1 or len(summaries) != 1
            or any(line.strip() for line in lines[summaries[0] + 1:])):
        raise PrewarmError("Single-case Rust log lacks one passing case and a terminal summary.")


def _run_command(argv: list[str], *, log: Path, env: dict[str, str], deadline: float,
                 artifact_dir: Path, timeout: float) -> None:
    timeout = min(timeout, normal.remaining_work(deadline))
    normal._run_command(argv, log_path=log, env=env, timeout=timeout,
                        artifact_dir=artifact_dir, cargo_artifact_monitor=True)


def _encode_summary(summary: dict[str, Any]) -> bytes:
    raw = (json.dumps(summary, sort_keys=True, indent=2, allow_nan=False) + "\n").encode("utf-8")
    if len(raw) > REPORT_BYTES:
        raise PrewarmError("Scalar-only summary exceeds 64 KiB.")
    return raw


def run(artifact_arg: str | None = None, local_config_arg: str | None = None) -> int:
    started = time.monotonic()
    artifacts: Path | None = None
    status, message, code = "failed", "Comparison requirements were not proven.", 1
    comparison = None
    try:
        artifacts = normal._fresh_artifact_dir(artifact_arg)
        logs, rust = artifacts / "logs", artifacts / "rust"
        normal._new_private_dir(logs)
        normal._new_private_dir(rust)
        normal._require_artifact_free_space(artifacts)
        env = normal.clean_environment()
        config = None
        if local_config_arg is not None:
            config = Path(local_config_arg).expanduser()
            if not config.is_absolute():
                raise PrewarmError("--local-config must be absolute.")
        total_deadline = _work_deadline(os.environ.get("MEDIA_NETWORK_E2E_JOB_WORK_DEADLINE_MONOTONIC"),
                                        started=started)
        normal._require_tools(env)
        local_build = normal._load_local_build()
        compile_deadline = min(started + COMPILE_BUDGET_SECONDS, total_deadline - RUNTIME_BUDGET_SECONDS - FINALIZE_BUDGET_SECONDS)
        invocation = 0
        def bounded_build_run(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
            nonlocal invocation
            invocation += 1
            log = logs / f"signed-compile-{invocation:02d}.log"
            available = ARTIFACT_BYTES - normal._artifact_size(artifacts)
            limit = min(COMMAND_LOG_BYTES, available)
            timeout = min(normal.remaining_work(total_deadline), compile_deadline - time.monotonic())
            if limit <= 0 or timeout <= 0:
                raise local_build.BuildError("Signed compile budget or artifact limit exhausted.")
            result = normal.ui.run_process(list(argv), log_path=log, timeout_seconds=timeout,
                    max_output_bytes=limit, env=normal.clean_environment(kwargs.get("env", os.environ)),
                    cwd=Path(kwargs.get("cwd", ROOT)))
            if result.timed_out or result.output_limited or (kwargs.get("check", True) and result.returncode):
                raise local_build.BuildError(f"Bounded signed compile failed; log: {log.name}")
            return subprocess.CompletedProcess(list(argv), result.returncode,
                    log.read_text(encoding="utf-8", errors="replace"), "")
        original_run = local_build.RUN
        controls = {}
        try:
            local_build.RUN = bounded_build_run
            with _prepared_signed_targets(local_build, config) as receipt:
                probe = normal._signed_product(receipt, "probe")
                binary = normal._signed_product(receipt, "media-tests")
                normal._mute_system_audio(logs, env)
                runtime_deadline = min(total_deadline - FINALIZE_BUDGET_SECONDS, time.monotonic() + RUNTIME_BUDGET_SECONDS)
                for mode in ("off", "on"):
                    remaining = normal.remaining_work(runtime_deadline)
                    if remaining < CASE_BUDGET_SECONDS:
                        raise PrewarmError("Insufficient runtime remains for the next fresh player invocation.")
                    case_env = dict(env)
                    case_env["LAN_MEDIA_ACCEPTANCE_PROBE_BINARY"] = str(probe)
                    case_env["LAN_MEDIA_ACCEPTANCE_ARTIFACTS"] = str(rust)
                    case_env["LAN_MEDIA_ACCEPTANCE_PREWARM"] = mode
                    _run_command(build_test_argv(binary, mode), log=logs / f"{mode}-test.log", env=case_env,
                                 deadline=runtime_deadline, artifact_dir=artifacts,
                                 timeout=min(CASE_BUDGET_SECONDS, remaining))
                    validate_single_case_log(logs / f"{mode}-test.log", TESTS[mode])
                    case = validate_report(rust / CASES[mode]["report"], mode)
                    controls[mode] = case
                comparison = _comparison(controls["off"], controls["on"])
        finally:
            local_build.RUN = original_run
        normal._artifact_size(artifacts)
        normal.remaining_work(total_deadline)
        status = "valid_comparison" if comparison["status"] == "valid_comparison" else "negative_diagnostic"
        message = "Both healthy single-player runs produced recovery high samples." if status == "valid_comparison" else "One or both runs censored recovery high; no latency delta is claimed."
        code = 0
    except (PrewarmError, normal.MediaNetworkError, normal.ui.UIE2EError, OSError, ValueError,
            TypeError, AttributeError, KeyError, subprocess.SubprocessError, local_build_error()) as error:
        message = f"{type(error).__name__}: {error}"[:1024]
        print(f"[media-prewarm-e2e] {message}", file=sys.stderr, flush=True)
    finally:
        if artifacts is not None:
            try:
                summary = {"schema_version": 1, "status": status, "diagnostic_only": True,
                           "message": " ".join(message.split())[:1024],
                           "elapsed_seconds": round(time.monotonic() - started, 3)}
                if comparison is not None:
                    summary["prewarm_comparison"] = comparison
                raw = _encode_summary(summary)
                if normal._artifact_size(artifacts) + len(raw) > ARTIFACT_BYTES:
                    raise PrewarmError("Bounded summary exceeds report/artifact limit.")
                (artifacts / "summary.json").write_bytes(raw)
            except (OSError, PrewarmError) as error:
                code = 1
                print(f"[media-prewarm-e2e] summary retention failed: {type(error).__name__}", file=sys.stderr)
    return code


def local_build_error() -> type[BaseException]:
    try:
        return normal._load_local_build().BuildError
    except Exception:
        return RuntimeError


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-dir", help="Empty directory for bounded diagnostic artifacts.")
    parser.add_argument("--local-config", help="Absolute public local signing config path.")
    args = parser.parse_args(argv)
    return run(args.artifact_dir, args.local_config)


if __name__ == "__main__":
    raise SystemExit(main())
