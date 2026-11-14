from __future__ import annotations

import importlib.util
import tempfile
import sys
import time
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("media-high-preparation-e2e.py")
SPEC = importlib.util.spec_from_file_location("media_high_preparation_e2e", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
runner = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = runner
SPEC.loader.exec_module(runner)


def arm(arm_name: str, *, eligible: bool = False, session_id: str | None = None) -> dict[str, object]:
    reader = {
        "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" if arm_name == "off" else "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "context_id": f"{arm_name}-context",
        "session_id": session_id or f"{arm_name}-session",
        "session_generation": 1,
    }
    value: dict[str, object] = {
        "schema_version": 1,
        "arm": arm_name,
        "status": "inconclusive" if arm_name == "on" and not eligible else "complete",
        "eligible": eligible,
        "transport_boundary": runner.TRANSPORT_BOUNDARY,
        "fixture_is_fresh": True,
        "cache_is_fresh": True,
        "reader": reader,
        "native": {
            "mode": "continuous_native_control_v1",
            "requested_duration_seconds": 600,
            "deadline_seconds": 720,
            "muted_verified": True,
            "system_audio_mute_verified": True,
            "automatic_waiting_enabled": True,
            "phase_completed": True,
            "high_low_high_decoded": True,
            "decode_censored": False,
            "seek_count": 0,
            "rate_change_count": 0,
            "item_replacement_count": 0,
            "buffer_cap_set": False,
            "challenge_sample_count": 3,
            "challenge_samples_truncated": False,
        },
        "plans": [],
        "origin_range_transfers": [],
        "player_range_transfers": [],
        "first_recovery_high_origin_request": None,
        "first_recovery_high_completed_source_body": None,
        "first_recovery_high_decoded_monotonic_seconds": None,
        "recovery_preparation": None,
        "inconclusive_reason": None,
        "plan_history_retention": {
            "observed_matching_candidates": 0,
            "internally_retained_records": 0,
            "internal_record_limit": runner.MAX_PLAN_RECORDS,
            "serialized_plan_records": 0,
            "serialization_record_limit": runner.MAX_SERIALIZED_PLAN_RECORDS,
            "producer_history_limit": runner.MAX_PLAN_RECORDS,
            "truncated": False,
        },
        "transfer_event_retention": {
            "origin_recovery_range_events_observed": 0,
            "origin_witnesses_retained": 0,
            "origin_ring_limit": runner.CAUSAL_EVENT_RING_LIMIT,
            "origin_ring_truncated": False,
            "player_recovery_range_events_observed": 0,
            "player_witnesses_retained": 0,
            "player_ring_limit": runner.CAUSAL_EVENT_RING_LIMIT,
            "player_ring_truncated": False,
        },
        "logical_range_counts": {
            "requested_resource_ranges": 0,
            "readable_resource_ranges": 0,
            "deferred_resource_ranges": 0,
        },
        "attribution": {
            "exclusive_attribution": "not_proven",
            "causal_faster_selection_claim": False,
        },
    }
    if eligible:
        spans = [
            {
                "kind": "Video",
                "resource_id": "video-resource",
                "variant_id": "high-variant",
                "context_id": reader["context_id"],
                "session_id": reader["session_id"],
                "session_generation": reader["session_generation"],
                "logical_window_start_seconds": 10.0,
                "logical_window_end_seconds": 30.0,
                "byte_range_start": 10,
                "byte_range_end_exclusive": 11,
                "before_read_state": "missing",
                "after_read_state": "readable",
                "newly_readable_bytes_lower_bound": 1,
                "readable": True,
            },
            {
                "kind": "Audio",
                "resource_id": "audio-resource",
                "variant_id": "high-variant",
                "context_id": reader["context_id"],
                "session_id": reader["session_id"],
                "session_generation": reader["session_generation"],
                "logical_window_start_seconds": 10.0,
                "logical_window_end_seconds": 30.0,
                "byte_range_start": 20,
                "byte_range_end_exclusive": 24,
                "before_read_state": "readable",
                "after_read_state": "readable",
                "newly_readable_bytes_lower_bound": 0,
                "readable": True,
            },
        ]
        value["plans"] = [{
            "plan_id": 1,
            "context_id": reader["context_id"],
            "reader_id": reader["id"],
            "session_id": reader["session_id"],
            "session_generation": reader["session_generation"],
            "window_start_seconds": 10.0,
            "window_end_seconds": 30.0,
            "initial_window_readable": False,
            "outcome": "Completed",
            "window_readable": True,
            "selected_spans": spans,
        }]
        origin_event = {
            "context_id": reader["context_id"],
            "reader_id": reader["id"],
            "session_id": reader["session_id"],
            "session_generation": reader["session_generation"],
            "variant_id": "high-variant",
            "resource_id": "video-resource",
            "logical_window_start_seconds": 10.0,
            "logical_window_end_seconds": 30.0,
            "range_start": 10,
            "range_end_exclusive": 11,
            "status_code": 206,
            "body_complete": True,
            "bytes_observed": 1,
            "request_started_monotonic_seconds": 12.0,
            "completed_monotonic_seconds": 13.0,
        }
        player_event = {
            **origin_event,
            "resource_id": "video-resource",
            "body_complete": None,
            "bytes_observed": None,
        }
        value["plans"][0]["admitted_sample_monotonic_seconds"] = 11.0
        value["recovery_preparation"] = {
            "plan_id": 1,
            "completed": True,
            "initial_window_readable": False,
            "window_readable": True,
            "initial_missing_to_final_readable": True,
            "newly_readable_bytes_lower_bound": 1,
            "newly_readable_semantics": runner.NEWLY_READABLE_SEMANTICS,
            "completed_origin_range_overlaps": True,
            "player_range_overlap_status": "yes",
            "window_start_seconds": 10.0,
            "window_end_seconds": 30.0,
            "admitted_sample_monotonic_seconds": 11.0,
            "terminal_observed_monotonic_seconds": 14.0,
            "selected_spans": spans,
        }
        value["origin_range_transfers"] = [origin_event]
        value["player_range_transfers"] = [player_event]
        value["plan_history_retention"].update({
            "observed_matching_candidates": 1,
            "internally_retained_records": 1,
            "serialized_plan_records": 1,
        })
        value["transfer_event_retention"].update({
            "origin_recovery_range_events_observed": 1,
            "origin_witnesses_retained": 1,
            "player_recovery_range_events_observed": 1,
            "player_witnesses_retained": 1,
        })
        value["first_recovery_high_completed_source_body"] = {
            "resource_id": "video-resource",
            "range_start": 10,
            "range_end_exclusive": 11,
            "status_code": 206,
            "body_complete": True,
            "bytes_observed": 1,
            "request_started_monotonic_seconds": 5.0,
            "completed_monotonic_seconds": 6.0,
        }
    return value


class MediaHighPreparationRunnerTests(unittest.TestCase):
    def test_probe_preflight_requires_cleanup_verification_self_test_case(self) -> None:
        binary = Path("/stable/probe")
        complete = (
            '{"status":"complete","cases":['
            '"high_preparation_mode_requires_bound_reader_and_fixed_phase_limits",'
            f'"{runner.PROBE_SELF_TEST_CASE}"]}}'
        )
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(runner, "_run_bounded", return_value=(0, complete)) as run_bounded:
                runner._verify_probe_self_test(
                    binary,
                    logs=Path(temporary),
                    env={},
                    deadline=time.monotonic() + 90,
                )
            self.assertEqual(run_bounded.call_args.args[0], [str(binary), "--self-test"])

            missing_case = complete.replace(f',"{runner.PROBE_SELF_TEST_CASE}"', "")
            with patch.object(runner, "_run_bounded", return_value=(0, missing_case)):
                with self.assertRaises(runner.HighPreparationError):
                    runner._verify_probe_self_test(
                        binary,
                        logs=Path(temporary),
                        env={},
                        deadline=time.monotonic() + 90,
                    )

    def test_opt_in_runner_has_bounded_budgets_and_exact_test_filter(self) -> None:
        self.assertEqual(
            (runner.TOTAL_BUDGET_SECONDS, runner.COMPILE_BUDGET_SECONDS, runner.RUNTIME_BUDGET_SECONDS,
             runner.FINALIZE_BUDGET_SECONDS, runner.ARTIFACT_BYTES),
            (3600, 1500, 1800, 60, 32 * 1024 * 1024),
        )
        self.assertEqual(
            runner.TEST_NAME,
            "media_high_preparation_native_tests::native_high_preparation_off_on_comparison",
        )
        self.assertEqual(runner.REPORT_BYTES, 64 * 1024)
        self.assertEqual(runner.MAX_ARTIFACT_ENTRIES, 256)

    def test_test_filter_queries_only_the_exact_named_entry_with_existing_cap(self) -> None:
        binary = Path("/stable/cache-server-tests")
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(runner, "_run_bounded", return_value=(0, f"{runner.TEST_NAME}: test\n")) as run_bounded:
                runner._verify_test_filter(
                    binary,
                    logs=Path(temporary),
                    env={},
                    deadline=runner.time.monotonic() + 90,
                )
        args, kwargs = run_bounded.call_args
        self.assertEqual(args[0], [str(binary), runner.TEST_NAME, "--exact", "--ignored", "--list"])
        self.assertEqual(kwargs["max_output_bytes"], 64 * 1024)
        with patch.object(runner, "_run_bounded", return_value=(0, f"{runner.TEST_NAME}: test\nextra_test: test\n")):
            with self.assertRaises(runner.HighPreparationError):
                runner._verify_test_filter(
                    binary,
                    logs=Path(temporary),
                    env={},
                    deadline=time.monotonic() + 90,
                )

    def test_legacy_ignored_filter_contains_exactly_the_existing_three_cases(self) -> None:
        network = runner._load_network_runner()
        binary = Path("/stable/cache-server-tests")
        listing = "\n".join(f"{name}: test" for name in network.EXPECTED_TESTS) + "\n"
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(runner, "_run_bounded", return_value=(0, listing)) as run_bounded:
                runner._verify_legacy_native_test_filter(
                    binary,
                    network=network,
                    logs=Path(temporary),
                    env={},
                    deadline=time.monotonic() + 90,
                )
            args, kwargs = run_bounded.call_args
            self.assertEqual(
                args[0],
                [
                    str(binary), "media_acceptance_tests::", "--ignored", "--list",
                    "--skip", network.EXTENDED_DIAGNOSTIC_TEST,
                ],
            )
            self.assertEqual(kwargs["max_output_bytes"], 64 * 1024)
            for invalid_listing in (
                "\n".join(f"{name}: test" for name in network.EXPECTED_TESTS[:-1]) + "\n",
                listing + f"{runner.TEST_NAME}: test\n",
                listing + f"{network.EXPECTED_TESTS[0]}: test\n",
            ):
                with patch.object(runner, "_run_bounded", return_value=(0, invalid_listing)):
                    with self.assertRaises(runner.HighPreparationError):
                        runner._verify_legacy_native_test_filter(
                            binary,
                            network=network,
                            logs=Path(temporary),
                            env={},
                            deadline=time.monotonic() + 90,
                        )

    def test_default_artifact_path_is_portable_for_normal_and_nested_worktrees(self) -> None:
        for root in (Path("/checkout/tvOS-net-player"), Path("/checkout/.codex-tmp/feature")):
            result = runner.default_artifact_root(root)
            self.assertEqual(result, root / "build" / "MediaHighPreparationE2E" / "artifacts")
            self.assertNotIn("hls-high-preparation-20261114", str(result))
            self.assertNotIn("native-worker", str(result))
        self.assertEqual(
            runner.resolve_artifact_root(None, root=Path("/clone")),
            Path("/clone/build/MediaHighPreparationE2E/artifacts"),
        )
        self.assertEqual(runner.resolve_artifact_root("/private/output"), Path("/private/output"))
        with self.assertRaises(runner.HighPreparationError):
            runner.resolve_artifact_root("relative/output")

    def test_artifact_base_preserves_existing_checkpoint_and_creates_private_run_child(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve() / "owned-output"
            base.mkdir(mode=0o700)
            checkpoint = base / "checkpoint.md"
            checkpoint.write_text("keep", encoding="utf-8")
            before_mode = base.stat().st_mode
            run_dir = runner._private_run_directory(base)
            self.assertTrue(run_dir.is_relative_to(base))
            self.assertEqual(checkpoint.read_text(encoding="utf-8"), "keep")
            self.assertEqual(base.stat().st_mode, before_mode)
            self.assertEqual(run_dir.stat().st_mode & 0o777, 0o700)

    def test_artifact_root_rejects_symlink_without_chmod_or_following_it(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            target = root / "target"
            target.mkdir()
            link = root / "link"
            link.symlink_to(target, target_is_directory=True)
            with self.assertRaises(runner.HighPreparationError):
                runner._private_run_directory(link)

    def test_runner_refuses_to_start_without_explicit_opt_in(self) -> None:
        with self.assertRaises(runner.HighPreparationError):
            runner.run(enabled=False)

    def test_arm_contract_accepts_only_bound_continuous_muted_report(self) -> None:
        runner.validate_arm_report(arm("off"), expected_arm="off")
        runner.validate_arm_report(arm("on", eligible=True), expected_arm="on")

    def test_arm_contract_rejects_mutated_player_controls(self) -> None:
        value = arm("on")
        value["native"]["seek_count"] = 1  # type: ignore[index]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_arm_report(value, expected_arm="on")

    def test_arm_contract_rejects_logical_range_counts_as_transfer_claims(self) -> None:
        value = arm("on")
        value["plans"] = [{"selected_spans": [], "downloaded_bytes": 12}]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_arm_report(value, expected_arm="on")

    def test_eligible_on_requires_new_readable_recovery_coverage_and_origin_activity(self) -> None:
        value = arm("on", eligible=True)
        value["recovery_preparation"]["initial_missing_to_final_readable"] = False  # type: ignore[index]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_arm_report(value, expected_arm="on")

    def test_eligible_on_does_not_recast_unknown_cache_state_as_missing(self) -> None:
        value = arm("on", eligible=True)
        value["recovery_preparation"]["selected_spans"][0]["before_read_state"] = "unknown"  # type: ignore[index]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_arm_report(value, expected_arm="on")

    def test_eligible_on_requires_both_selected_audio_and_video_ranges(self) -> None:
        value = arm("on", eligible=True)
        video_only = value["recovery_preparation"]["selected_spans"][:1]  # type: ignore[index]
        value["recovery_preparation"]["selected_spans"] = video_only  # type: ignore[index]
        value["plans"][0]["selected_spans"] = video_only  # type: ignore[index]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_arm_report(value, expected_arm="on")

    def test_eligible_on_rejects_plan_history_truncation_and_retention_mismatch(self) -> None:
        value = arm("on", eligible=True)
        value["plan_history_retention"]["truncated"] = True  # type: ignore[index]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_arm_report(value, expected_arm="on")
        value = arm("on", eligible=True)
        value["transfer_event_retention"]["origin_witnesses_retained"] = 0  # type: ignore[index]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_arm_report(value, expected_arm="on")

    def test_eligible_on_requires_matching_reader_bound_player_ingress(self) -> None:
        value = arm("on", eligible=True)
        value["player_range_transfers"][0]["reader_id"] = "wrong-reader"  # type: ignore[index]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_arm_report(value, expected_arm="on")

    def test_comparison_requires_fresh_sequential_arms_and_never_claims_causality(self) -> None:
        summary = runner.validate_comparison(arm("off"), arm("on", eligible=True))
        self.assertEqual(summary["comparison_status"], "eligible")
        self.assertEqual(summary["exclusive_attribution"], "not_proven")
        self.assertFalse(summary["causal_faster_selection_claim"])
        self.assertFalse(summary["latency_negative_evidence"])

    def test_ineligible_on_is_explicitly_inconclusive_not_latency_failure(self) -> None:
        summary = runner.validate_comparison(arm("off"), arm("on"))
        self.assertEqual(summary["comparison_status"], "inconclusive")
        self.assertFalse(summary["latency_negative_evidence"])
        self.assertEqual(runner.comparison_exit_status(summary["comparison_status"]), ("inconclusive", 2))

    def test_eligible_comparison_exit_is_only_for_a_valid_on_arm(self) -> None:
        summary = runner.validate_comparison(arm("off"), arm("on", eligible=True))
        self.assertEqual(runner.comparison_exit_status(summary["comparison_status"]), ("complete", 0))

    def test_comparison_rejects_reused_reader_or_session(self) -> None:
        off = arm("off")
        on = arm("on")
        on["reader"]["id"] = off["reader"]["id"]  # type: ignore[index]
        with self.assertRaises(runner.HighPreparationError):
            runner.validate_comparison(off, on)


if __name__ == "__main__":
    unittest.main()
