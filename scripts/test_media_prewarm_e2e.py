from __future__ import annotations

import copy
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("media-prewarm-e2e.py")
# Exact complete Rust-serde exports, kept inline so this test is checkout-portable.
RUST_CACHE_HIT_V4 = r'''{"schema_version":4,"mode":"on","boundary_policy":"unbuffered_after_20s_safety","recovery_snapshot":{"schema_version":1,"observed_monotonic_seconds":100.0,"playhead_seconds":60.0,"loaded_end_seconds":80.0},"target_window_start_seconds":80.0,"target_window_end_seconds":100.0,"byte_budget":8388608,"planned_bytes":32,"transfer_byte_budget":16777216,"planned_transfer_bytes":524288,"attempt_count":0,"attempted_span_bytes":0,"attempted_ranges":[],"cache_published_bytes":0,"cache_published_spans":[],"preempted_operation_count":0,"foreground_defer_count":0,"elapsed_seconds":0.050900833,"coverage_semantics":"sum_of_fully_readable_selected_ranges","cache_covered_bytes_before":32,"cache_covered_spans_before":[{"resource_id":"audio.m4s","start":0,"end":32}],"cache_covered_bytes_after":32,"cache_covered_spans_after":[{"resource_id":"audio.m4s","start":0,"end":32}],"selected_ranges":[{"resource_id":"audio.m4s","start":0,"end":32}],"transfer_plan":[{"resource_id":"audio.m4s","start":0,"end":524288}],"resource_ids":{"video":"video.m4s","audio":"audio.m4s"},"resource_total_lengths":{"video":1048576,"audio":1048576},"completed":true,"failure":{"category":null,"resource_id":null,"start":null,"end":null,"upstream_status":null},"provenance":{"shared_origin_counter_scope":"playback_and_preparation","attempt_bytes_semantics":"sum_of_requested_range_lengths_not_wire_bytes","identity_probe_attempts_included":true,"transfer_plan_semantics":"empty_cache_upper_bound_startup_prefix_and_64k_playback_preparation_spans","expanded_transfer_may_overlap_buffered_media":true}}'''
RUST_TYPED_503_V4 = r'''{"schema_version":4,"mode":"on","boundary_policy":"unbuffered_after_20s_safety","recovery_snapshot":{"schema_version":1,"observed_monotonic_seconds":100.0,"playhead_seconds":60.0,"loaded_end_seconds":80.0},"target_window_start_seconds":80.0,"target_window_end_seconds":100.0,"byte_budget":8388608,"planned_bytes":32,"transfer_byte_budget":16777216,"planned_transfer_bytes":524288,"attempt_count":1,"attempted_span_bytes":524288,"attempted_ranges":[{"resource_id":"audio.m4s","resource_total_length":1048576,"start":0,"end":524288,"purpose":"cache_fill","status_code":503,"headers_validated":false,"body_validated":null,"identity_validated":null,"cache_published":false,"outcome":"upstream_status"}],"cache_published_bytes":0,"cache_published_spans":[],"preempted_operation_count":0,"foreground_defer_count":0,"elapsed_seconds":0.022279917,"coverage_semantics":"sum_of_fully_readable_selected_ranges","cache_covered_bytes_before":0,"cache_covered_spans_before":[],"cache_covered_bytes_after":0,"cache_covered_spans_after":[],"selected_ranges":[{"resource_id":"audio.m4s","start":0,"end":32}],"transfer_plan":[{"resource_id":"audio.m4s","start":0,"end":524288}],"resource_ids":{"video":"video.m4s","audio":"audio.m4s"},"resource_total_lengths":{"video":1048576,"audio":1048576},"completed":false,"failure":{"category":"upstream_status","resource_id":"audio.m4s","start":0,"end":524288,"upstream_status":503},"provenance":{"shared_origin_counter_scope":"playback_and_preparation","attempt_bytes_semantics":"sum_of_requested_range_lengths_not_wire_bytes","identity_probe_attempts_included":true,"transfer_plan_semantics":"empty_cache_upper_bound_startup_prefix_and_64k_playback_preparation_spans","expanded_transfer_may_overlap_buffered_media":true}}'''
SPEC = importlib.util.spec_from_file_location("media_prewarm_e2e_under_test", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
prewarm = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prewarm)


def sample(display: float, elapsed: float, monotonic: float) -> dict:
    return {"display_time_seconds": display, "observed_elapsed_seconds": elapsed,
            "observed_monotonic_seconds": monotonic}


def make_report(mode: str, *, recovered: bool = True) -> dict:
    suffix = "control" if mode == "off" else "treatment"
    case = prewarm.TESTS[mode]
    recovery_samples = [sample(40 + i, 40 + i, 140 + i) for i in range(3)] if recovered else []
    entries = [
        {"expected_width": 640, "expected_height": 360, "matched": True,
         "distinct_frame_count": 3, "required_distinct_frame_count": 3,
         "samples": [sample(1 + i, 10 + i, 110 + i) for i in range(3)]},
        {"expected_width": 320, "expected_height": 180, "matched": True,
         "distinct_frame_count": 3, "required_distinct_frame_count": 3,
         "samples": [sample(20 + i, 26 + i, 126 + i) for i in range(3)]},
        {"expected_width": 640, "expected_height": 360, "matched": recovered,
         "distinct_frame_count": 3 if recovered else 0, "required_distinct_frame_count": 3,
         "diagnostic_optional_recovery_high": True, "samples": recovery_samples},
    ]
    timeline_samples = []
    for elapsed, monotonic in ((0, 100), (10, 110), (30, 130), (40, 140)):
        timeline_samples.append({"elapsed_seconds": elapsed, "observed_monotonic_seconds": monotonic,
            "playhead_seconds": elapsed + 1, "loaded_end_seconds": elapsed + 10,
            "buffered_ahead_seconds": 9, "decoded_width": 640, "decoded_height": 360,
            "time_control_status": "playing", "indicated_bitrate": 1000000})
    resource_totals = {"video": 2_000_000, "audio": 2_000_000}
    resource_ids = {"video": "video-id", "audio": "audio-id"}
    selected_ranges = [{"resource_id": "audio-id", "start": 50, "end": 250},
                       {"resource_id": "video-id", "start": 700_000, "end": 700_800}]
    transfer_plan = [{"resource_id": "audio-id", "start": 0, "end": prewarm.STARTUP_PREFIX_BYTES},
                     {"resource_id": "video-id", "start": 0, "end": prewarm.STARTUP_PREFIX_BYTES},
                     {"resource_id": "video-id", "start": 655_360, "end": 720_896}]
    attempt_templates = [
        {"resource_id": span["resource_id"], "resource_total_length": resource_totals[
            "audio" if span["resource_id"] == "audio-id" else "video"],
         "start": span["start"], "end": span["end"], "purpose": "cache_fill",
         "status_code": 206, "headers_validated": True, "body_validated": True,
         "identity_validated": None, "cache_published": True, "outcome": "cache_published"}
        for span in selected_ranges
    ]
    prewarm_contract = {
        "schema_version": 4, "mode": "off" if mode == "off" else "on",
        "boundary_policy": "unbuffered_after_20s_safety",
        "recovery_snapshot": {"schema_version": 1, "observed_monotonic_seconds": 135,
                              "playhead_seconds": 100, "loaded_end_seconds": 110},
        "target_window_start_seconds": 130, "target_window_end_seconds": 150,
        "byte_budget": prewarm.PREWARM_BYTES, "planned_bytes": 1000,
        "transfer_byte_budget": prewarm.PREWARM_TRANSFER_BYTES,
        "planned_transfer_bytes": sum(span["end"] - span["start"] for span in transfer_plan),
        "resource_ids": resource_ids, "resource_total_lengths": resource_totals,
        "selected_ranges": selected_ranges, "transfer_plan": transfer_plan,
        "coverage_semantics": "sum_of_fully_readable_selected_ranges",
        "attempt_count": 0 if mode == "off" else len(attempt_templates),
        "attempted_span_bytes": 0 if mode == "off" else 1000,
        "attempted_ranges": [] if mode == "off" else attempt_templates,
        "cache_published_bytes": 0 if mode == "off" else 1000,
        "cache_published_spans": [] if mode == "off" else copy.deepcopy(selected_ranges),
        "preempted_operation_count": 0,
        "foreground_defer_count": 0,
        "failure": {"category": None, "resource_id": None, "start": None, "end": None,
                    "upstream_status": None},
        "elapsed_seconds": 0 if mode == "off" else 1.5,
        "cache_covered_bytes_before": 0,
        "cache_covered_spans_before": [],
        "cache_covered_bytes_after": 0 if mode == "off" else 1000,
        "cache_covered_spans_after": [] if mode == "off" else copy.deepcopy(selected_ranges),
        "completed": True,
        "provenance": copy.deepcopy(prewarm.PROVENANCE),
    }
    return {
        "schema_version": 1, "status": "passed", "case_full_name": case,
        "scenario": prewarm.CASES[mode]["scenario"], "source_duration_seconds": 3600,
        "diagnostic_only": True,
        "probe": {
            "schema_version": 1, "status": "completed", "requested_duration_seconds": 600,
            "overall_deadline_seconds": 720, "deadline_mode": "cooperative",
            "elapsed_seconds": 615,
            "diagnostic_observation_window": {"schema_version": 1,
                "started_monotonic_seconds": 100, "ended_monotonic_seconds": 700,
                "requested_duration_seconds": 600, "controls_excluded": True,
                "decoded_frame_continuity": {"schema_version": 1, "decoded_frame_count": 90,
                    "observed_interval_count": 89, "maximum_interframe_gap_seconds": 0.04,
                    "gap_threshold_seconds": 0.5, "large_gap_count": 0,
                    "observation_clock_anomaly_count": 0,
                    "coverage": "between_observed_decoded_frames_only",
                    "interpretation": "decoded_frame_observation_gaps_not_visible_black_screen_acceptance"}},
            "decoded_frame_count": 90, "audio_track_present": True, "enabled_audio_track_count": 1,
            "steps": [{"name": name, "status": "completed",
                       **({"elapsed_seconds": 600} if name == "play" else {})} for name in (
                "play", "pause", "resume", "seek_forward", "seek_forward_play", "seek_back",
                "seek_back_play", "rate_1_25", "duration_remainder")],
            "diagnostic_optional_recovery_high": True,
            "video_size_expectations": {"enabled": True, "complete": recovered,
                "semantics": "ordered_decoded_size_subsequence",
                "minimum_distinct_display_timestamps_per_size": 3, "entries": entries},
            "playback_timeline": {"sample_interval_seconds": 10, "maximum_samples": 96,
                "truncated": False, "samples": timeline_samples},
            "recovery_observation": {"observed_monotonic_seconds": 136,
                "player_state": {"playhead_seconds": 100,
                    "preferred_forward_buffer_duration_seconds": 0,
                    "automatically_waits_to_minimize_stalling": True,
                    "loaded_time_ranges": {"truncated": False, "source_count": 1,
                        "valid_count": 1, "invalid_count": 0,
                        "observations": [{"start_seconds": 90, "duration_seconds": 20,
                                          "end_seconds": 110}]}}},
            "decoded_frame_continuity": {"schema_version": 1, "decoded_frame_count": 90,
                "observed_interval_count": 89, "maximum_interframe_gap_seconds": 0.04,
                "gap_threshold_seconds": 0.5, "large_gap_count": 0,
                "observation_clock_anomaly_count": 0,
                "coverage": "between_observed_decoded_frames_only",
                "interpretation": "decoded_frame_observation_gaps_not_visible_black_screen_acceptance"},
        },
        "requests": {"media_payload_by_phase_and_representation": [[100, 0, 0], [0, 100, 0], [0, 0, 0]],
            "count_by_phase_and_representation": [[1, 0, 0], [1, 1, 0], [0, 0, 0]],
            "successful_by_representation": [1, 1, 0], "high_fault_status_count": 1,
            "first_recovery_high_range_request": None, "first_recovery_high_media_request": None},
        "source_duration_seconds": 3600,
        "fault_timeline": {"enabled": True, "bad_high_status_code": 503,
            "decoder_ready_monotonic_seconds": 101, "baseline_start_monotonic_seconds": 102,
            "bad_high_before_monotonic_seconds": 115, "bad_high_after_monotonic_seconds": 125,
            "recovery_before_monotonic_seconds": 130, "recovery_after_monotonic_seconds": 135},
        "lan_ingress": {"count_by_phase_and_representation": [[1, 0, 0], [0, 1, 0],
            [1 if recovered else 0, 0, 0]]},
        "prewarm_comparison": prewarm_contract,
    }


class PrewarmRunnerTests(unittest.TestCase):
    def validate_fixture(self, report: dict, mode: str) -> dict:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            path.write_text(json.dumps(report, allow_nan=True))
            return prewarm.validate_report(path, mode)

    def assert_report_rejected(self, report: dict, mode: str = "on") -> None:
        with self.assertRaises(prewarm.PrewarmError):
            self.validate_fixture(report, mode)

    def test_exact_single_case_commands(self) -> None:
        binary = Path("/signed/media-tests")
        for mode, name in prewarm.TESTS.items():
            argv = prewarm.build_test_argv(binary, mode)
            self.assertEqual(argv[0], str(binary))
            self.assertEqual(argv[1], name)
            self.assertEqual(argv[2:], ["--exact", "--ignored", "--nocapture", "--test-threads=1", "--color=never"])
        with self.assertRaises(prewarm.PrewarmError):
            prewarm.build_test_argv(binary, "unknown")

    def test_normal_three_case_contract_is_not_mutated(self) -> None:
        self.assertEqual(len(prewarm.normal.EXPECTED_TESTS), 3)
        self.assertEqual(len(prewarm.normal.EXPECTED_CASES), 3)
        self.assertEqual(prewarm.normal.REPORT_BYTES, 64 * 1024)
        self.assertEqual(prewarm.normal.ARTIFACT_BYTES, 32 * 1024 * 1024)
        self.assertNotIn(prewarm.TESTS["off"], prewarm.normal.EXPECTED_TESTS)
        self.assertNotIn(prewarm.TESTS["on"], prewarm.normal.EXPECTED_CASES)

    def test_prepares_only_fixed_signed_targets(self) -> None:
        local_build = mock.Mock()
        context = object()
        local_build.prepared_targets.return_value = context
        self.assertIs(prewarm._prepared_signed_targets(local_build, Path("/private/config.json")), context)
        local_build.prepared_targets.assert_called_once_with({"probe", "media-tests"},
                                                             config_file=Path("/private/config.json"))

    def test_budget_constants_are_finite_and_bounded(self) -> None:
        self.assertEqual(prewarm.TOTAL_BUDGET_SECONDS, 3600)
        self.assertEqual(prewarm.COMPILE_BUDGET_SECONDS, 1500)
        self.assertEqual(prewarm.RUNTIME_BUDGET_SECONDS, 1800)
        self.assertEqual(prewarm.FINALIZE_BUDGET_SECONDS, 60)
        self.assertEqual(prewarm.CASE_BUDGET_SECONDS, 840)
        self.assertEqual(prewarm.COMMAND_LOG_BYTES, 8 * 1024 * 1024)
        self.assertEqual(prewarm.ARTIFACT_BYTES, 32 * 1024 * 1024)
        self.assertEqual(prewarm.REPORT_BYTES, 64 * 1024)

    def test_total_deadline_uses_local_sixty_minute_budget(self) -> None:
        with mock.patch.object(prewarm.time, "monotonic", return_value=100):
            self.assertEqual(prewarm._work_deadline(None, started=10), 3610)
            self.assertEqual(prewarm._work_deadline("200", started=10), 200)
            for value in ("nan", "inf", "bad", "99"):
                with self.subTest(value=value), self.assertRaises(prewarm.PrewarmError):
                    prewarm._work_deadline(value, started=10)

    def test_native_supervisor_receives_deadline_and_aggregate_artifact_guard(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifacts = Path(directory)
            with mock.patch.object(prewarm.normal, "remaining_work", return_value=90), \
                    mock.patch.object(prewarm.normal, "_run_command") as run_command:
                prewarm._run_command(["/signed/test", "case"], log=artifacts / "case.log",
                                     env={"PATH": "/usr/bin"}, deadline=500,
                                     artifact_dir=artifacts, timeout=120)
            self.assertEqual(run_command.call_args.kwargs["timeout"], 90)
            self.assertTrue(run_command.call_args.kwargs["cargo_artifact_monitor"])
            self.assertEqual(run_command.call_args.kwargs["log_path"], artifacts / "case.log")

    def test_valid_control_and_treatment_reports(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            validated = {}
            for mode in ("off", "on"):
                path = Path(directory) / prewarm.CASES[mode]["report"]
                path.write_text(json.dumps(make_report(mode)))
                validated[mode] = prewarm.validate_report(path, mode)
                self.assertEqual(validated[mode]["mode"], mode)
            comparison = prewarm._comparison(validated["off"], validated["on"])
        self.assertEqual(comparison["status"], "valid_comparison")
        self.assertEqual(comparison["schema_version"], 2)
        self.assertEqual(comparison["prewarm_report_schema_version"], 4)
        self.assertIsNotNone(comparison["recovery_high"]["raw_observed_latency_delta_seconds"])
        self.assertFalse(comparison["acceptance_passed"])
        self.assertIn("not_origin_prewarm", comparison["client_high_request_count"]["source"])
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["planned_logical_selected_bytes"], 1000)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["planned_expanded_transfer_bytes"],
                         2 * prewarm.STARTUP_PREFIX_BYTES + prewarm.PREPARATION_SPAN_BYTES)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["actual_attempt_count"], 2)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["actual_attempted_span_bytes"], 1000)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["cache_published_span_count"], 2)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["cache_published_bytes"], 1000)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["preempted_operation_count"], 0)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["covered_logical_selected_bytes"], 1000)
        self.assertEqual(comparison["prewarm_readiness"]["attempt_bytes_semantics"],
                         "sum_of_requested_range_lengths_not_wire_bytes")

    def test_v4_plan_is_exact_and_optional_spans_need_not_be_filled(self) -> None:
        report = make_report("on")
        comparison = report["prewarm_comparison"]
        self.assertEqual(comparison["selected_ranges"][1]["start"], 700_000)
        self.assertEqual(comparison["transfer_plan"][2],
                         {"resource_id": "video-id", "start": 655_360, "end": 720_896})
        self.assertGreater(comparison["planned_transfer_bytes"], comparison["cache_published_bytes"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "treatment.json"
            path.write_text(json.dumps(report))
            validated = prewarm.validate_report(path, "on")
        self.assertEqual(validated["prewarm"]["planned_bytes"], 1000)
        self.assertEqual(validated["prewarm"]["cache_covered_bytes_after"], 1000)

    def test_attempt_failure_can_be_followed_by_successful_cache_coverage(self) -> None:
        report = make_report("on")
        contract = report["prewarm_comparison"]
        failed = copy.deepcopy(contract["attempted_ranges"][0])
        failed.update(status_code=503, headers_validated=False, body_validated=None,
                      cache_published=None, outcome="upstream_status")
        contract["attempted_ranges"].insert(0, failed)
        contract.update(attempt_count=3, attempted_span_bytes=1200)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "treatment.json"
            path.write_text(json.dumps(report))
            validated = prewarm.validate_report(path, "on")
        self.assertEqual(validated["prewarm"]["attempt_count"], 3)
        self.assertEqual(validated["prewarm"]["cache_published_bytes"], 1000)

    def test_schema_one_selected_only_reports_are_refused(self) -> None:
        report = make_report("on")
        report["prewarm_comparison"]["schema_version"] = 1
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "old.json"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_schema_two_reports_are_not_accepted_as_compatibility_input(self) -> None:
        report = make_report("on")
        report["prewarm_comparison"]["schema_version"] = 2
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "schema-two.json"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_schema_three_is_never_reinterpreted_as_v4(self) -> None:
        report = make_report("on")
        report["prewarm_comparison"]["schema_version"] = 3
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "schema-three.json"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_v4_exact_keys_enums_and_exact_integer_types(self) -> None:
        mutations = [
            lambda c: c.update(unexpected="value"),
            lambda c: c.pop("resource_total_lengths"),
            lambda c: c.update(schema_version=True),
            lambda c: c.update(attempt_count=True),
            lambda c: c.update(preempted_operation_count=True),
            lambda c: c["attempted_ranges"][0].update(purpose="download"),
            lambda c: c["attempted_ranges"][0].update(outcome="success"),
            lambda c: c["attempted_ranges"][0].update(headers_validated=1),
            lambda c: c["attempted_ranges"][0].update(status_code=True),
            lambda c: c["attempted_ranges"][0].update(start=prewarm.U64_MAX + 1),
        ]
        for mutate in mutations:
            report = make_report("on")
            mutate(report["prewarm_comparison"])
            with self.subTest(mutation=mutate):
                self.assert_report_rejected(report)

        report = make_report("on")
        report["prewarm_comparison"]["elapsed_seconds"] = float("nan")
        self.assert_report_rejected(report)

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nan.json"
            path.write_text(json.dumps(make_report("on"), allow_nan=True).replace(
                '"elapsed_seconds": 1.5', '"elapsed_seconds": NaN', 1))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_transfer_plan_contract_rejects_malformed_or_underaccounted_plans(self) -> None:
        mutations = [
            lambda c: c.update(planned_transfer_bytes=prewarm.PREWARM_TRANSFER_BYTES + 1),
            lambda c: c.update(transfer_byte_budget=prewarm.PREWARM_TRANSFER_BYTES + 1),
            lambda c: c.update(planned_transfer_bytes=369),
            lambda c: c.update(operation_requested_bytes=1000),
            lambda c: c.update(operation_count=2),
            lambda c: c.update(transfer_plan=[c["selected_ranges"][0], c["selected_ranges"][1]]),
            lambda c: c.update(transfer_plan=[{"resource_id": "unknown", "start": 0, "end": 500},
                                               c["transfer_plan"][0], c["transfer_plan"][1]]),
            lambda c: c.update(transfer_plan=[{"resource_id": "audio-id", "start": 0, "end": 120},
                                               {"resource_id": "video-id", "start": 0, "end": 200},
                                               {"resource_id": "video-id", "start": 150, "end": 250}]),
            lambda c: c.update(transfer_plan=[{"resource_id": "audio-id", "start": 0, "end": 120},
                                               {"resource_id": "video-id", "start": 150, "end": 250},
                                               {"resource_id": "video-id", "start": 0, "end": 150}]),
            lambda c: c.update(selected_ranges=[{"resource_id": "audio-id", "start": 5, "end": 4},
                                                c["selected_ranges"][1]]),
            lambda c: c.update(selected_ranges=[{"resource_id": "audio-id", "start": 0,
                                                 "end": prewarm.U64_MAX},
                                                {"resource_id": "video-id", "start": 0,
                                                 "end": prewarm.U64_MAX}]),
            lambda c: c.update(transfer_plan=[{"resource_id": "audio-id", "start": 0, "end": 120},
                                               {"resource_id": "video-id", "start": 0, "end": 150},
                                               {"resource_id": "video-id", "start": 150, "end": prewarm.U64_MAX}]),
            lambda c: c.update(coverage_semantics="wire_download_bytes"),
            lambda c: c.update(resource_ids={"video": "same", "audio": "same"}),
            lambda c: c.update(source_url="https://private.invalid/media"),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            for mutation in mutations:
                report = make_report("on")
                mutation(report["prewarm_comparison"])
                path.write_text(json.dumps(report))
                with self.subTest(mutation=mutation), self.assertRaises(prewarm.PrewarmError):
                    prewarm.validate_report(path, "on")

    def test_control_keeps_zero_attempts_and_publications_with_policy_plan(self) -> None:
        report = make_report("off")
        contract = report["prewarm_comparison"]
        self.assertEqual(contract["attempted_span_bytes"], 0)
        self.assertEqual(contract["attempt_count"], 0)
        self.assertEqual(contract["cache_published_bytes"], 0)
        self.assertEqual(contract["cache_published_spans"], [])
        self.assertEqual(contract["planned_transfer_bytes"],
                         2 * prewarm.STARTUP_PREFIX_BYTES + prewarm.PREPARATION_SPAN_BYTES)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "control.json"
            path.write_text(json.dumps(report))
            validated = prewarm.validate_report(path, "off")
        self.assertEqual(len(validated["prewarm"]["transfer_plan"]), 3)
        self.assertEqual(validated["prewarm"]["attempted_span_bytes"], 0)
        self.assertEqual(validated["prewarm"]["cache_published_bytes"], 0)

    def test_v4_rejects_attempt_ledger_and_byte_accounting_forgery(self) -> None:
        cases = [
            ("attempt bytes undercharged", lambda c: c.update(attempted_span_bytes=999)),
            ("attempt count forged", lambda c: c.update(attempt_count=3)),
            ("cache fill outside plan union", lambda c: c["attempted_ranges"][0].update(
                start=1_500_000, end=1_500_200)),
            ("unknown resource", lambda c: c["attempted_ranges"][0].update(resource_id="unknown")),
            ("unknown outcome", lambda c: c["attempted_ranges"][0].update(outcome="retry")),
            ("failed outcome claims publication", lambda c: c["attempted_ranges"][0].update(
                outcome="network")),
            ("missing attempt field", lambda c: c["attempted_ranges"][0].pop("status_code")),
            ("published byte counter forged", lambda c: c.update(cache_published_bytes=999)),
            ("negative preemption counter", lambda c: c.update(preempted_operation_count=-1)),
            ("negative admission counter", lambda c: c.update(foreground_defer_count=-1)),
            ("root failure on completed report", lambda c: c["failure"].update(category="network")),
        ]
        for label, mutate in cases:
            report = make_report("on")
            mutate(report["prewarm_comparison"])
            with self.subTest(case=label):
                self.assert_report_rejected(report)

    def test_control_rejects_each_nonzero_intervention_or_transfer_field(self) -> None:
        fields = ("attempt_count", "attempted_span_bytes", "cache_published_bytes",
                  "preempted_operation_count", "foreground_defer_count")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "control.json"
            for field in fields:
                report = make_report("off")
                report["prewarm_comparison"][field] = 1
                path.write_text(json.dumps(report))
                with self.subTest(field=field), self.assertRaises(prewarm.PrewarmError):
                    prewarm.validate_report(path, "off")
            report = make_report("off")
            contract = report["prewarm_comparison"]
            contract["attempted_ranges"] = copy.deepcopy(make_report("on")["prewarm_comparison"]["attempted_ranges"])
            contract["attempt_count"] = 2
            contract["attempted_span_bytes"] = 1000
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "off")

    def test_attempt_ledger_counts_discovery_and_identity_probe_sends(self) -> None:
        report = make_report("on")
        contract = report["prewarm_comparison"]
        contract["attempted_ranges"].extend([
            {"resource_id": "audio-id", "resource_total_length": 2_000_000,
             "start": 0, "end": 1, "purpose": "resource_discovery", "status_code": 206,
             "headers_validated": True, "body_validated": True,
             "identity_validated": None, "cache_published": None, "outcome": "body_validated"},
            {"resource_id": "video-id", "resource_total_length": 2_000_000,
             "start": 1_500_000, "end": 1_500_001, "purpose": "identity_probe", "status_code": 206,
             "headers_validated": True, "body_validated": True,
             "identity_validated": True, "cache_published": None, "outcome": "body_validated"},
        ])
        contract.update(attempt_count=4, attempted_span_bytes=1002)
        validated = self.validate_fixture(report, "on")
        self.assertEqual(validated["prewarm"]["attempt_count"], 4)
        self.assertEqual(validated["prewarm"]["attempted_span_bytes"], 1002)

        report = make_report("on")
        contract = report["prewarm_comparison"]
        probe = {"resource_id": "video-id", "resource_total_length": 2_000_000,
                 "start": 2_000_000, "end": 2_000_001, "purpose": "identity_probe",
                 "status_code": 206, "headers_validated": True, "body_validated": True,
                 "identity_validated": True, "cache_published": None, "outcome": "body_validated"}
        contract["attempted_ranges"].append(probe)
        contract.update(attempt_count=3, attempted_span_bytes=1001)
        self.assert_report_rejected(report)

        report = make_report("on")
        report["prewarm_comparison"]["attempted_ranges"][0]["resource_total_length"] += 1
        self.assert_report_rejected(report)

    def test_zero_send_cache_hit_and_admission_deferrals_do_not_invent_attempts(self) -> None:
        report = make_report("on")
        contract = report["prewarm_comparison"]
        contract.update(attempt_count=0, attempted_span_bytes=0, attempted_ranges=[],
                        cache_published_bytes=0, cache_published_spans=[],
                        preempted_operation_count=4, foreground_defer_count=3)
        validated = self.validate_fixture(report, "on")
        self.assertEqual(validated["prewarm"]["attempted_ranges"], [])
        self.assertEqual(validated["prewarm"]["cache_published_spans"], [])
        self.assertEqual(validated["prewarm"]["cache_covered_bytes_after"], 1000)

    def test_actual_rust_serde_failure_state_matches_typed_attempt_ledger(self) -> None:
        cache_hit = json.loads(RUST_CACHE_HIT_V4)
        failed = json.loads(RUST_TYPED_503_V4)
        old_cache_hit = copy.deepcopy(cache_hit)
        old_cache_hit.update(target_window_start_seconds=60.0,
                             target_window_end_seconds=80.0)
        cache_failure = cache_hit["failure"]
        prewarm._validate_failure_state(cache_failure, cache_hit["attempted_ranges"],
                                        set(cache_hit["resource_ids"].values()),
                                        completed=cache_hit["completed"], mode="on",
                                        report_name="treatment-cache-hit.v4.json")
        self.assertIs(prewarm._prewarm_contract({"prewarm_comparison": cache_hit}, "on",
                                                "treatment-cache-hit.v4.json"), cache_hit)
        with self.assertRaisesRegex(prewarm.PrewarmError, "20-second safety boundary"):
            prewarm._prewarm_contract({"prewarm_comparison": old_cache_hit}, "on",
                                      "old-treatment-cache-hit.v4.json")
        failure = failed["failure"]
        prewarm._validate_failure_state(failure, failed["attempted_ranges"],
                                        set(failed["resource_ids"].values()),
                                        completed=failed["completed"], mode="on",
                                        report_name="treatment-loopback-503.v4.json")
        self.assertEqual(failure, {"category": "upstream_status", "resource_id": "audio.m4s",
                                   "start": 0, "end": 524288, "upstream_status": 503})
        self.assertFalse(failed["completed"])
        self.assertEqual(failed["cache_published_spans"], [])
        self.assertEqual(failed["cache_published_bytes"], 0)
        with self.assertRaisesRegex(prewarm.PrewarmError, "not a completed case"):
            prewarm._prewarm_contract({"prewarm_comparison": failed}, "on",
                                      "treatment-loopback-503.v4.json")
        malformed_failure = copy.deepcopy(failure)
        malformed_failure["category"] = ["upstream_status"]
        with self.assertRaises(prewarm.PrewarmError):
            prewarm._validate_failure_state(malformed_failure, failed["attempted_ranges"],
                                            set(failed["resource_ids"].values()),
                                            completed=False, mode="on",
                                            report_name="treatment-loopback-503.v4.json")

    def test_published_union_merges_overlapping_and_adjacent_attempts(self) -> None:
        for first_end, second_start, attempted_bytes in ((180, 100, 1080), (150, 150, 1000)):
            with self.subTest(first_end=first_end, second_start=second_start):
                report = make_report("on")
                contract = report["prewarm_comparison"]
                original = contract["attempted_ranges"][0]
                first, second = copy.deepcopy(original), copy.deepcopy(original)
                first["end"] = first_end
                second["start"] = second_start
                contract["attempted_ranges"] = [first, second, contract["attempted_ranges"][1]]
                contract.update(attempt_count=3, attempted_span_bytes=attempted_bytes)
                validated = self.validate_fixture(report, "on")
                self.assertEqual(validated["prewarm"]["cache_published_bytes"], 1000)

    def test_published_spans_must_equal_union_not_double_count_or_infer(self) -> None:
        report = make_report("on")
        contract = report["prewarm_comparison"]
        duplicate = copy.deepcopy(contract["cache_published_spans"][0])
        duplicate["start"] = 100
        contract["cache_published_spans"].insert(1, duplicate)
        contract["cache_published_bytes"] = 1100
        self.assert_report_rejected(report)

        report = make_report("on")
        contract = report["prewarm_comparison"]
        contract["attempted_ranges"][0]["cache_published"] = False
        contract["attempted_ranges"][0]["outcome"] = "io"
        contract["cache_published_spans"] = [contract["cache_published_spans"][1]]
        contract["cache_published_bytes"] = 800
        validated = self.validate_fixture(report, "on")
        self.assertEqual(validated["prewarm"]["cache_published_bytes"], 800)

    def test_validation_flags_and_lifecycle_outcomes_are_consistent(self) -> None:
        mutations = [
            lambda a: a.update(headers_validated=False, body_validated=True),
            lambda a: a.update(status_code=None, headers_validated=True),
            lambda a: a.update(body_validated=1),
            lambda a: a.update(cache_published=True, body_validated=False),
            lambda a: a.update(outcome="cache_published", cache_published=False),
            lambda a: a.update(outcome="identity_changed", identity_validated=None),
            lambda a: a.update(outcome="upstream_status", status_code=None,
                               headers_validated=None, body_validated=None,
                               cache_published=None),
            lambda a: a.update(purpose="unknown"),
            lambda a: a.update(outcome="unknown"),
            lambda a: a.update(status_code=True),
            lambda a: a.update(resource_total_length=prewarm.U64_MAX + 1),
        ]
        for mutate in mutations:
            report = make_report("on")
            mutate(report["prewarm_comparison"]["attempted_ranges"][0])
            with self.subTest(mutation=mutate):
                self.assert_report_rejected(report)

    def test_discovery_total_may_be_unknown_only_before_validation(self) -> None:
        report = make_report("on")
        contract = report["prewarm_comparison"]
        discovery = {"resource_id": "video-id", "resource_total_length": None,
                     "start": 0, "end": 1, "purpose": "resource_discovery", "status_code": 503,
                     "headers_validated": False, "body_validated": None,
                     "identity_validated": None, "cache_published": None, "outcome": "upstream_status"}
        contract["attempted_ranges"].append(discovery)
        contract.update(attempt_count=3, attempted_span_bytes=1001)
        self.validate_fixture(report, "on")

        report = make_report("on")
        contract = report["prewarm_comparison"]
        discovery["headers_validated"] = True
        discovery["outcome"] = "headers_validated"
        contract["attempted_ranges"].append(discovery)
        contract.update(attempt_count=3, attempted_span_bytes=1001)
        self.assert_report_rejected(report)

    def test_coverage_is_observed_exact_selected_ranges_and_nulls_pair(self) -> None:
        invalid_reports = []
        report = make_report("on")
        report["prewarm_comparison"].update(cache_covered_bytes_after=None,
                                             cache_covered_spans_after=None)
        invalid_reports.append(report)
        report = make_report("on")
        report["prewarm_comparison"]["cache_covered_spans_after"] = None
        invalid_reports.append(report)
        report = make_report("on")
        report["prewarm_comparison"]["cache_covered_spans_after"] = [
            {"resource_id": "video-id", "start": 700_000, "end": 700_799},
            {"resource_id": "audio-id", "start": 50, "end": 250}]
        invalid_reports.append(report)
        report = make_report("on")
        report["prewarm_comparison"]["cache_covered_bytes_after"] = 999
        invalid_reports.append(report)
        report = make_report("on")
        report["prewarm_comparison"]["cache_covered_spans_after"] = [
            {"resource_id": "audio-id", "start": 50, "end": 250},
            {"resource_id": "audio-id", "start": 100, "end": 200}]
        invalid_reports.append(report)
        for invalid in invalid_reports:
            with self.subTest(report=invalid["prewarm_comparison"]["cache_covered_spans_after"]):
                self.assert_report_rejected(invalid)

        report = make_report("on")
        report["prewarm_comparison"].update(cache_covered_bytes_before=None,
                                             cache_covered_spans_before=None)
        self.validate_fixture(report, "on")

    def test_resource_ids_lengths_and_total_bounds_are_strict(self) -> None:
        invalid_ids = (".", "..", "video/track", "video\\track", "video id",
                       "https://example.invalid/a", "é", "x" * 129)
        for resource_id in invalid_ids:
            report = make_report("on")
            report["prewarm_comparison"]["resource_ids"]["video"] = resource_id
            with self.subTest(resource_id=resource_id):
                self.assert_report_rejected(report)
        for invalid_total in (0, True, prewarm.U64_MAX + 1):
            report = make_report("on")
            report["prewarm_comparison"]["resource_total_lengths"]["video"] = invalid_total
            with self.subTest(total=invalid_total):
                self.assert_report_rejected(report)
        report = make_report("on")
        report["prewarm_comparison"]["resource_total_lengths"].pop("audio")
        self.assert_report_rejected(report)

    def test_attempt_and_elapsed_limits_are_independent_from_logical_plans(self) -> None:
        report = make_report("on")
        contract = report["prewarm_comparison"]
        template = copy.deepcopy(contract["attempted_ranges"][0])
        template.update(start=0, end=prewarm.STARTUP_PREFIX_BYTES,
                        status_code=None, headers_validated=None, body_validated=None,
                        identity_validated=None, cache_published=None, outcome="network")
        contract["attempted_ranges"] = [copy.deepcopy(template) for _ in range(33)]
        contract.update(attempt_count=33,
                        attempted_span_bytes=33 * prewarm.STARTUP_PREFIX_BYTES,
                        cache_published_bytes=0, cache_published_spans=[])
        self.assert_report_rejected(report)

        report = make_report("on")
        contract = report["prewarm_comparison"]
        contract["attempted_ranges"] *= 65
        contract["attempt_count"] = len(contract["attempted_ranges"])
        contract["attempted_span_bytes"] = sum(
            item["end"] - item["start"] for item in contract["attempted_ranges"])
        self.assert_report_rejected(report)

        for key, value in (("attempt_count", True), ("preempted_operation_count", -1),
                           ("foreground_defer_count", -1), ("elapsed_seconds", 15.001),
                           ("elapsed_seconds", float("nan"))):
            report = make_report("on")
            report["prewarm_comparison"][key] = value
            with self.subTest(key=key, value=value):
                self.assert_report_rejected(report)

    def test_explicit_diagnostic_and_summary_caps_are_separate(self) -> None:
        self.assertEqual(prewarm.REPORT_BYTES, 64 * 1024)
        self.assertEqual(prewarm.normal.REPORT_BYTES, 64 * 1024)
        self.assertEqual(prewarm.normal.ARTIFACT_BYTES, 32 * 1024 * 1024)
        self.assertEqual(prewarm.PREWARM_OBJECT_BYTES, 192 * 1024)
        self.assertEqual(prewarm.PREWARM_REPORT_BYTES, 256 * 1024)

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            report = make_report("on")
            report["padding"] = ""
            base_size = len(json.dumps(report).encode("utf-8"))
            padding = prewarm.PREWARM_REPORT_BYTES - base_size
            report["padding"] = "x" * padding
            path.write_text(json.dumps(report))
            self.assertEqual(path.stat().st_size, prewarm.PREWARM_REPORT_BYTES)
            prewarm._read_json(path)
            report["padding"] += "x"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm._read_json(path)

            report = make_report("on")
            contract = report["prewarm_comparison"]
            contract["padding"] = ""
            encoded = lambda: json.dumps(contract, sort_keys=True, separators=(",", ":"),
                                         ensure_ascii=True, allow_nan=False).encode("utf-8")
            contract["padding"] = "x" * (prewarm.PREWARM_OBJECT_BYTES - len(encoded()))
            path.write_text(json.dumps(report))
            prewarm._read_json(path)
            contract["padding"] += "x"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm._read_json(path)

        base = prewarm._encode_summary({"padding": ""})
        exact_summary = {"padding": "x" * (prewarm.REPORT_BYTES - len(base))}
        self.assertEqual(len(prewarm._encode_summary(exact_summary)), prewarm.REPORT_BYTES)
        with self.assertRaises(prewarm.PrewarmError):
            prewarm._encode_summary({"padding": exact_summary["padding"] + "x"})

    def test_failed_diagnostic_is_not_accepted_as_a_censored_success(self) -> None:
        report = make_report("on", recovered=False)
        report["status"] = "failed"
        report["prewarm_comparison"].update(
            completed=False,
            failure={"category": "network", "resource_id": "video-id", "start": 0,
                     "end": 500, "upstream_status": None},
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "failed.json"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_single_case_log_requires_exact_case_and_terminal_one_pass_summary(self) -> None:
        case = prewarm.TESTS["on"]
        summary = ("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
                   "7 filtered out; finished in 1.25s")
        valid = ("running 1 test\nnoise from --nocapture\n"
                 f"test {case} ... ok\n{summary}\n")
        inline_diagnostic = ("running 1 test\n"
                             f"test {case} ... HLS foreground range upstream returned 503 Service Unavailable\n"
                             "retrying through standalone path\n"
                             "ok\n"
                             f"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
                             "7 filtered out; finished in 1.25s\n")
        bare_header = ("running 1 test\n"
                       f"test {case} ...\n"
                       "HLS foreground range upstream returned 503\n"
                       "retrying through standalone path\n"
                       "ok\n"
                       f"{summary}\n")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "case.log"
            for content in (valid, inline_diagnostic, bare_header):
                with self.subTest(accepted_log=content):
                    path.write_text(content)
                    prewarm.validate_single_case_log(path, case)

            invalid_logs = {
                "zero passed summary": valid.replace("1 passed", "0 passed"),
                "missing case header": valid.replace(f"test {case} ... ok\n", ""),
                "missing standalone ok": (f"test {case} ... upstream returned 503\n{summary}\n"),
                "failure": (f"test {case} ... FAILED\n{summary}\n"),
                "ignored": (f"test {case} ... ignored\n{summary}\n"),
                "bench": (f"test {case} ... bench\n{summary}\n"),
                "ambiguous status prefix": (f"test {case} ... ok but not terminal\n{summary}\n"),
                "ambiguous failure prefix": (f"test {case} ... FAILED after setup\n{summary}\n"),
                "ambiguous ignored prefix": (f"test {case} ... ignored after setup\n{summary}\n"),
                "ambiguous bench prefix": (f"test {case} ... bench details\n{summary}\n"),
                "standalone failure while pending": (
                    f"test {case} ... HLS upstream returned 503\nFAILED\n{summary}\n"),
                "duplicate case": (f"test {case} ... ok\ntest {case} ... ok\n{summary}\n"),
                "unexpected case": (f"test other::case ... ok\n{summary}\n"),
                "interleaved header": (f"test {case} ... warning\ntest other::case ... ok\n{summary}\n"),
                "orphan status": (f"ok\ntest {case} ... ok\n{summary}\n"),
                "premature summary": (f"{summary}\ntest {case} ... ok\n"),
                "duplicated summary": (f"test {case} ... ok\n{summary}\n{summary}\n"),
                "trailing output": (f"test {case} ... ok\n{summary}\nlate output\n"),
            }
            for label, content in invalid_logs.items():
                path.write_text(content)
                with self.subTest(rejected_log=label), self.assertRaises(prewarm.PrewarmError):
                    prewarm.validate_single_case_log(path, case)

            path.write_text(valid)
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_single_case_log(path, case, returncode=1)
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_single_case_log(path, case, timed_out=True)

    def test_missing_recovery_is_censored_not_zero(self) -> None:
        off, on = make_report("off", recovered=False), make_report("on")
        with tempfile.TemporaryDirectory() as directory:
            off_path, on_path = Path(directory) / "off.json", Path(directory) / "on.json"
            off_path.write_text(json.dumps(off))
            on_path.write_text(json.dumps(on))
            comparison = prewarm._comparison(prewarm.validate_report(off_path, "off"),
                                             prewarm.validate_report(on_path, "on"))
        self.assertEqual(comparison["status"], "negative_diagnostic")
        self.assertIsNone(comparison["recovery_high"]["control_latency_bounds"])
        self.assertIsNone(comparison["recovery_high"]["raw_observed_latency_delta_seconds"])

    def test_bad_fields_and_wrong_mode_are_refused(self) -> None:
        mutations = [
            lambda r: r.update(status="failed"),
            lambda r: r.update(diagnostic_only=False),
            lambda r: r["prewarm_comparison"].update(byte_budget=prewarm.PREWARM_BYTES + 1),
            lambda r: r["prewarm_comparison"].update(planned_bytes=prewarm.PREWARM_BYTES + 1),
            lambda r: r["prewarm_comparison"].update(target_window_start_seconds=119),
            lambda r: r["prewarm_comparison"].update(elapsed_seconds=16),
            lambda r: r["prewarm_comparison"].update(operation_requested_bytes=0),
            lambda r: r["prewarm_comparison"].update(target_window_end_seconds=3601),
            lambda r: r["prewarm_comparison"]["recovery_snapshot"].update(observed_monotonic_seconds=134),
            lambda r: r["prewarm_comparison"]["recovery_snapshot"].update(playhead_seconds=float("nan")),
            lambda r: r["probe"]["diagnostic_observation_window"]["decoded_frame_continuity"].update(
                observation_clock_anomaly_count=-1),
            lambda r: r["lan_ingress"].update(count_by_phase_and_representation=[[0]]),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            for mutation in mutations:
                report = make_report("on")
                mutation(report)
                path.write_text(json.dumps(report, allow_nan=True))
                with self.subTest(mutation=mutation), self.assertRaises(prewarm.PrewarmError):
                    prewarm.validate_report(path, "on")
            path.write_text(json.dumps(make_report("off")))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_optional_recovery_requires_explicit_empty_or_valid_triple(self) -> None:
        report = make_report("off", recovered=False)
        entry = report["probe"]["video_size_expectations"]["entries"][2]
        entry.pop("diagnostic_optional_recovery_high")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "control.json"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "off")
            entry["diagnostic_optional_recovery_high"] = True
            entry["samples"] = [sample(1, 1, 140)]
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "off")

            report = make_report("off", recovered=False)
            report["probe"]["diagnostic_optional_recovery_high"] = False
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "off")

    def test_stale_recovery_phase_is_refused(self) -> None:
        report = make_report("on")
        report["probe"]["video_size_expectations"]["entries"][2]["samples"][0]["observed_monotonic_seconds"] = 134
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "treatment.json"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_observation_window_rejects_missing_malformed_or_overdeadline_bounds(self) -> None:
        mutations = [
            lambda r: r["probe"].pop("diagnostic_observation_window"),
            lambda r: r["probe"]["diagnostic_observation_window"].update(ended_monotonic_seconds=210),
            lambda r: r["probe"]["diagnostic_observation_window"].update(started_monotonic_seconds=float("nan")),
            lambda r: r["probe"]["diagnostic_observation_window"].update(
                started_monotonic_seconds=701, ended_monotonic_seconds=700),
            lambda r: r["probe"]["diagnostic_observation_window"].update(ended_monotonic_seconds=800),
            lambda r: r["probe"]["diagnostic_observation_window"].update(schema_version=True),
            lambda r: r["probe"]["diagnostic_observation_window"].update(controls_excluded=1),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "window.json"
            for mutation in mutations:
                report = make_report("on")
                mutation(report)
                path.write_text(json.dumps(report, allow_nan=True))
                with self.subTest(mutation=mutation), self.assertRaises(prewarm.PrewarmError):
                    prewarm.validate_report(path, "on")

    def test_observation_window_allows_slow_startup_but_obeys_probe_bounds(self) -> None:
        report = make_report("on")
        probe = report["probe"]
        for sample_value in probe["playback_timeline"]["samples"]:
            sample_value["elapsed_seconds"] += 10
            sample_value["observed_monotonic_seconds"] += 10
        window = probe["diagnostic_observation_window"]
        window.update(started_monotonic_seconds=110, ended_monotonic_seconds=710)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "slow-startup.json"
            path.write_text(json.dumps(report))
            validated = prewarm.validate_report(path, "on")
            self.assertEqual(validated["observation_window"]["probe_started_monotonic_seconds"], 100)

            for start, end in ((99.999, 699.999), (115.5, 715.001)):
                invalid = copy.deepcopy(report)
                invalid["probe"]["diagnostic_observation_window"].update(
                    started_monotonic_seconds=start, ended_monotonic_seconds=end)
                path.write_text(json.dumps(invalid))
                with self.subTest(start=start, end=end), self.assertRaises(prewarm.PrewarmError):
                    prewarm.validate_report(path, "on")

    def test_control_phase_samples_must_remain_inside_window(self) -> None:
        report = make_report("on")
        report["probe"]["video_size_expectations"]["entries"][0]["samples"][0][
            "observed_monotonic_seconds"] = 701
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "window.json"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_pre_warm_recovery_evidence_cannot_claim_after_window(self) -> None:
        for evidence in ("recovery_high", "snapshot"):
            report = make_report("on")
            if evidence == "recovery_high":
                report["probe"]["video_size_expectations"]["entries"][2]["samples"][0][
                    "observed_monotonic_seconds"] = 701
            else:
                report["prewarm_comparison"]["recovery_snapshot"]["observed_monotonic_seconds"] = 701
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "window.json"
                path.write_text(json.dumps(report))
                with self.subTest(evidence=evidence), self.assertRaises(prewarm.PrewarmError):
                    prewarm.validate_report(path, "on")

    def test_short_initial_play_and_skipped_controls_are_refused(self) -> None:
        for change in ("short_play", "skipped_seek"):
            report = make_report("on")
            if change == "short_play":
                next(step for step in report["probe"]["steps"] if step["name"] == "play")[
                    "elapsed_seconds"] = 110
            else:
                next(step for step in report["probe"]["steps"] if step["name"] == "seek_forward")[
                    "status"] = "skipped"
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "window.json"
                path.write_text(json.dumps(report))
                with self.subTest(change=change), self.assertRaises(prewarm.PrewarmError):
                    prewarm.validate_report(path, "on")

    def test_duplicate_nonfinite_oversized_and_symlink_reports_fail(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "report.json"
            path.write_text('{"schema_version":1,"schema_version":1}')
            with self.assertRaises(prewarm.PrewarmError):
                prewarm._read_json(path)
            path.write_text('{"value":NaN}')
            with self.assertRaises(prewarm.PrewarmError):
                prewarm._read_json(path)
            path.write_text(" " * (prewarm.REPORT_BYTES + 1))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm._read_json(path)
            path.unlink()
            target = root / "target.json"
            target.write_text("{}")
            path.symlink_to(target)
            with self.assertRaises(prewarm.PrewarmError):
                prewarm._read_json(path)

    def test_client_high_ingress_is_required_for_observed_decode(self) -> None:
        report = make_report("on")
        report["lan_ingress"]["count_by_phase_and_representation"][2][0] = 0
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "treatment.json"
            path.write_text(json.dumps(report))
            with self.assertRaises(prewarm.PrewarmError):
                prewarm.validate_report(path, "on")

    def test_continuity_counts_and_gap_nullability(self) -> None:
        valid = make_report("off")["probe"]
        prewarm._continuity(valid, "x.json")
        for scope in ("global", "window"):
            for field, value in (("decoded_frame_count", -1), ("observed_interval_count", 90),
                                 ("large_gap_count", 90), ("observation_clock_anomaly_count", 1),
                                 ("maximum_interframe_gap_seconds", float("nan"))):
                bad = copy.deepcopy(valid)
                continuity = (bad["decoded_frame_continuity"] if scope == "global" else
                              bad["diagnostic_observation_window"]["decoded_frame_continuity"])
                continuity[field] = value
                with self.subTest(scope=scope, field=field), self.assertRaises(prewarm.PrewarmError):
                    prewarm._diagnostic_window(bad, "x.json")

        bad_null = copy.deepcopy(valid)
        bad_null["decoded_frame_continuity"].update(
            decoded_frame_count=0, observed_interval_count=0, maximum_interframe_gap_seconds=0.0)
        with self.assertRaises(prewarm.PrewarmError):
            prewarm._diagnostic_window(bad_null, "x.json")

        bad_window_total = copy.deepcopy(valid)
        bad_window_total["diagnostic_observation_window"]["decoded_frame_continuity"][
            "decoded_frame_count"] = 91
        with self.assertRaises(prewarm.PrewarmError):
            prewarm._diagnostic_window(bad_window_total, "x.json")

        bad_global_scalar = copy.deepcopy(valid)
        bad_global_scalar["decoded_frame_count"] = 91
        with self.assertRaises(prewarm.PrewarmError):
            prewarm._diagnostic_window(bad_global_scalar, "x.json")


if __name__ == "__main__":
    unittest.main()
