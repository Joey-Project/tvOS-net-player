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
PREWARM_BYTES = 8 * 1024 * 1024
PREWARM_TRANSFER_BYTES = 16 * 1024 * 1024
MAX_PREWARM_RANGES = 128
MAX_RESOURCE_ID_LENGTH = 256
U64_MAX = 2**64 - 1
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
    if path.is_symlink() or not path.is_file() or path.stat().st_size > REPORT_BYTES:
        raise PrewarmError(f"Report is missing, non-regular, or over 64 KiB: {path.name}")
    try:
        value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=normal._unique_object,
                           parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite JSON")))
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
        raise PrewarmError(f"Report JSON is invalid: {path.name}") from error
    if not isinstance(value, dict):
        raise PrewarmError(f"Report must be an object: {path.name}")
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


def _range_list(value: Any, resource_ids: set[str], name: str, report_name: str) -> list[dict[str, Any]]:
    if not isinstance(value, list) or not 1 <= len(value) <= MAX_PREWARM_RANGES:
        raise PrewarmError(f"{name} must contain 1-128 canonical ranges: {report_name}")
    ranges: list[dict[str, Any]] = []
    previous: tuple[str, int, int] | None = None
    for item in value:
        if (not isinstance(item, dict) or set(item) != {"resource_id", "start", "end"}
                or not isinstance(item.get("resource_id"), str)
                or not item["resource_id"] or len(item["resource_id"]) > MAX_RESOURCE_ID_LENGTH
                or item["resource_id"].strip() != item["resource_id"]
                or item["resource_id"] not in resource_ids
                or not _exact_int(item.get("start")) or not _exact_int(item.get("end"))
                or item["start"] < 0 or item["end"] > U64_MAX or item["end"] <= item["start"]):
            raise PrewarmError(f"{name} contains an invalid range: {report_name}")
        current = (item["resource_id"], item["start"], item["end"])
        if previous is not None:
            if current < previous or (current[0] == previous[0] and current[1] < previous[2]):
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


def _selected_ranges_covered(selected: list[dict[str, Any]], transfer: list[dict[str, Any]]) -> bool:
    by_resource: dict[str, list[dict[str, Any]]] = {}
    for item in transfer:
        by_resource.setdefault(item["resource_id"], []).append(item)
    for wanted in selected:
        cursor = wanted["start"]
        for item in by_resource.get(wanted["resource_id"], []):
            if item["end"] <= cursor:
                continue
            if item["start"] > cursor:
                break
            cursor = item["end"]
            if cursor >= wanted["end"]:
                break
        if cursor < wanted["end"]:
            return False
    return True


def _prewarm_contract(report: dict[str, Any], mode: str, report_name: str) -> dict[str, Any]:
    value = report.get("prewarm_comparison")
    expected_mode = "off" if mode == "off" else "on"
    if (not isinstance(value, dict) or not _exact_int(value.get("schema_version"), 2)
            or value.get("mode") != expected_mode
            or value.get("boundary_policy") != "unbuffered_after_20s_safety"):
        raise PrewarmError(f"Prewarm comparison identity is invalid: {report_name}")
    snapshot = value.get("recovery_snapshot")
    if (not isinstance(snapshot, dict) or not _exact_int(snapshot.get("schema_version"), 1)
            or any(not _finite(snapshot.get(key)) for key in
                   ("observed_monotonic_seconds", "playhead_seconds", "loaded_end_seconds"))
            or snapshot["observed_monotonic_seconds"] <= 0 or snapshot["playhead_seconds"] < 0
            or snapshot["playhead_seconds"] > 3600 or snapshot["loaded_end_seconds"] < snapshot["playhead_seconds"]
            or snapshot["loaded_end_seconds"] > 3600):
        raise PrewarmError(f"Recovery snapshot is invalid: {report_name}")
    start, end = value.get("target_window_start_seconds"), value.get("target_window_end_seconds")
    safe_start = max(snapshot["playhead_seconds"] + 20.0, snapshot["loaded_end_seconds"])
    if (not _finite(start) or not _finite(end) or start < safe_start or end <= start or end > 3600
            or end - start > WINDOW_MAX_SECONDS):
        raise PrewarmError(f"Target window violates the unbuffered 20-second safety boundary: {report_name}")
    budget, planned = value.get("byte_budget"), value.get("planned_bytes")
    transfer_budget, planned_transfer = value.get("transfer_byte_budget"), value.get("planned_transfer_bytes")
    requested, operations = value.get("operation_requested_bytes"), value.get("operation_count")
    elapsed = value.get("elapsed_seconds")
    provenance = value.get("provenance")
    id_map = value.get("resource_ids")
    if (not isinstance(id_map, dict) or set(id_map) != {"video", "audio"}
            or any(not isinstance(item, str) or not item or len(item) > MAX_RESOURCE_ID_LENGTH
                   or item.strip() != item for item in id_map.values())
            or id_map["video"] == id_map["audio"]):
        raise PrewarmError(f"Prewarm resource identities are invalid: {report_name}")
    ids = set(id_map.values())
    selected = _range_list(value.get("selected_ranges"), ids, "selected_ranges", report_name)
    transfer = _range_list(value.get("transfer_plan"), ids, "transfer_plan", report_name)
    selected_sum = _range_bytes(selected, "selected_ranges", report_name)
    transfer_sum = _range_bytes(transfer, "transfer_plan", report_name)
    if (not _exact_int(budget, PREWARM_BYTES) or not _exact_int(planned) or planned <= 0 or planned > budget
            or planned != selected_sum
            or not _exact_int(transfer_budget, PREWARM_TRANSFER_BYTES)
            or not _exact_int(planned_transfer) or planned_transfer <= 0 or planned_transfer > transfer_budget
            or planned_transfer != transfer_sum
            or not _selected_ranges_covered(selected, transfer)
            or any(not any(r["resource_id"] == resource_id and r["start"] == 0 for r in transfer)
                   for resource_id in ids)
            or value.get("coverage_semantics") != "sum_of_fully_readable_selected_ranges"
            or not _exact_int(requested) or not _exact_int(operations) or not _finite(elapsed)
            or elapsed < 0 or elapsed > 15 or value.get("completed") is not True
            or not isinstance(provenance, dict)
            or provenance.get("origin_counters_include_playback_and_prewarm") is not True
            or provenance.get("transfer_accounting") != "planned_background_parent_chunks_and_startup_not_wire_bytes"
            or provenance.get("expanded_transfer_may_overlap_buffered_media") is not True
            or any(key in value for key in ("source_url", "url", "credentials", "headers", "cookies"))):
        raise PrewarmError(f"Prewarm bounds, completion, or provenance are invalid: {report_name}")
    if mode == "off" and (requested != 0 or operations != 0):
        raise PrewarmError("Control report claims prewarm operations.")
    if mode == "on" and (requested != planned_transfer or requested > transfer_budget
                          or operations != len(transfer)):
        raise PrewarmError("Treatment report lacks bounded prewarm operations.")
    coverage = value.get("cache_covered_bytes_before"), value.get("cache_covered_bytes_after")
    if any(not _exact_int(item) or item < 0 or item > planned for item in coverage):
        raise PrewarmError(f"Selected-window cache coverage is invalid: {report_name}")
    if mode == "on" and coverage[1] != planned:
        raise PrewarmError("Treatment cache does not fully cover the selected planned ranges.")
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
        "schema_version": 1,
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
            "transfer_accounting": "planned_background_parent_chunks_and_startup_not_wire_bytes",
            "expanded_transfer_may_overlap_buffered_media": True,
            "control": {"planned_logical_selected_bytes": off["prewarm"]["planned_bytes"],
                        "planned_expanded_transfer_bytes": off["prewarm"]["planned_transfer_bytes"],
                        "operation_count": off["prewarm"]["operation_count"]},
            "treatment": {"planned_logical_selected_bytes": on["prewarm"]["planned_bytes"],
                          "planned_expanded_transfer_bytes": on["prewarm"]["planned_transfer_bytes"],
                          "operation_count": on["prewarm"]["operation_count"],
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
                raw = (json.dumps(summary, sort_keys=True, indent=2, allow_nan=False) + "\n").encode()
                if len(raw) > REPORT_BYTES or normal._artifact_size(artifacts) + len(raw) > ARTIFACT_BYTES:
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
