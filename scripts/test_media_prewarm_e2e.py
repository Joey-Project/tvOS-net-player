from __future__ import annotations

import copy
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("media-prewarm-e2e.py")
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
    prewarm_contract = {
        "schema_version": 2, "mode": "off" if mode == "off" else "on",
        "boundary_policy": "unbuffered_after_20s_safety",
        "recovery_snapshot": {"schema_version": 1, "observed_monotonic_seconds": 135,
                              "playhead_seconds": 100, "loaded_end_seconds": 110},
        "target_window_start_seconds": 130, "target_window_end_seconds": 150,
        "byte_budget": prewarm.PREWARM_BYTES, "planned_bytes": 1000,
        "transfer_byte_budget": prewarm.PREWARM_TRANSFER_BYTES, "planned_transfer_bytes": 1250,
        "resource_ids": {"video": "video-id", "audio": "audio-id"},
        "selected_ranges": [{"resource_id": "audio-id", "start": 50, "end": 250},
                            {"resource_id": "video-id", "start": 100, "end": 900}],
        "transfer_plan": [{"resource_id": "audio-id", "start": 0, "end": 250},
                          {"resource_id": "video-id", "start": 0, "end": 500},
                          {"resource_id": "video-id", "start": 500, "end": 1000}],
        "coverage_semantics": "sum_of_fully_readable_selected_ranges",
        "operation_requested_bytes": 0 if mode == "off" else 1250,
        "operation_count": 0 if mode == "off" else 3,
        "elapsed_seconds": 0 if mode == "off" else 1.5,
        "cache_covered_bytes_before": 0,
        "cache_covered_bytes_after": 0 if mode == "off" else 1000,
        "completed": True,
        "provenance": {"origin_counters_include_playback_and_prewarm": True,
                        "transfer_accounting": "planned_background_parent_chunks_and_startup_not_wire_bytes",
                        "expanded_transfer_may_overlap_buffered_media": True},
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
        self.assertIsNotNone(comparison["recovery_high"]["raw_observed_latency_delta_seconds"])
        self.assertFalse(comparison["acceptance_passed"])
        self.assertIn("not_origin_prewarm", comparison["client_high_request_count"]["source"])
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["planned_logical_selected_bytes"], 1000)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["planned_expanded_transfer_bytes"], 1250)
        self.assertEqual(comparison["prewarm_readiness"]["treatment"]["operation_count"], 3)

    def test_schema_two_accepts_selection_spanning_adjacent_parent_chunks(self) -> None:
        report = make_report("on")
        comparison = report["prewarm_comparison"]
        self.assertEqual(comparison["selected_ranges"][1]["start"], 100)
        self.assertEqual(comparison["transfer_plan"][1]["end"], comparison["transfer_plan"][2]["start"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "treatment.json"
            path.write_text(json.dumps(report))
            validated = prewarm.validate_report(path, "on")
        self.assertEqual(validated["prewarm"]["planned_bytes"], 1000)
        self.assertEqual(validated["prewarm"]["planned_transfer_bytes"], 1250)

    def test_schema_one_selected_only_reports_are_refused(self) -> None:
        report = make_report("on")
        report["prewarm_comparison"]["schema_version"] = 1
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "old.json"
            path.write_text(json.dumps(report))
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

    def test_control_keeps_zero_intervention_and_full_transfer_plan(self) -> None:
        report = make_report("off")
        contract = report["prewarm_comparison"]
        self.assertEqual(contract["operation_requested_bytes"], 0)
        self.assertEqual(contract["operation_count"], 0)
        self.assertEqual(contract["planned_transfer_bytes"], 1250)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "control.json"
            path.write_text(json.dumps(report))
            validated = prewarm.validate_report(path, "off")
        self.assertEqual(len(validated["prewarm"]["transfer_plan"]), 3)

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
