from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


SCRIPT = Path(__file__).with_name("media-network-e2e.py")
SPEC = importlib.util.spec_from_file_location("media_network_e2e", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
media = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = media
SPEC.loader.exec_module(media)

# Exact complete values from synthetic-token catalog joey-private-v3.
SYNTHETIC_ACCESS_TOKEN_ID = "access-a"
SYNTHETIC_ACCESS_TOKEN = "codex_synth_v1_access_a"
SYNTHETIC_API_KEY_ID = "api-key-a"
SYNTHETIC_API_KEY = "codex_synth_v1_api_key_a"


def report_payload(
    sizes: tuple[tuple[int, int], ...], *, scenario: str = "unbuffered_recovery", duration: int = 3600,
    probe_duration: int = 600, probe_deadline: int = 720,
) -> dict[str, object]:
    timeline_playhead = min(100.0, float(duration) - 0.1)
    timeline_end = min(timeline_playhead + 20.0, float(duration))
    timeline_offsets = (0.0, 10.0, 20.0, 30.0, 35.0) if duration == 45 else (*range(0, 101, 10), 111.5)
    entries = []
    for entry_index, (width, height) in enumerate(sizes):
        entries.append({
            "expected_width": width,
            "expected_height": height,
            "matched": True,
            "distinct_frame_count": 3,
            "required_distinct_frame_count": 3,
            "samples": [
                {
                    "display_time_seconds": float(index),
                    "observed_elapsed_seconds": float(entry_index * 3 + index) + 0.25,
                    "observed_monotonic_seconds": float((95, 110, 202)[entry_index] + index),
                }
                for index in range(1, 4)
            ],
        })
    return {
        "schema_version": 1,
        "status": "passed",
        "case_full_name": media.EXPECTED_TESTS[0],
        "scenario": scenario,
        "source_duration_seconds": duration,
        "diagnostic_only": False,
        "hls_playlist_preflight": {
            "bandwidth_semantics": "rfc8216_contiguous_duration_window",
            "high_variant_peak_bandwidth_bits_per_second": 1_200_000,
            "advertised_high_bandwidth_bits_per_second": 1_200_000,
        },
        "probe": {
            "schema_version": 1,
            "status": "completed",
            "requested_duration_seconds": probe_duration,
            "overall_deadline_seconds": probe_deadline,
            "deadline_mode": "cooperative",
            "decoded_frame_count": 9,
            "audio_track_present": True,
            "enabled_audio_track_count": 1,
            "steps": [
                {"name": name, "status": "completed", "elapsed_seconds": 110.0 if name == "play" else 1.0}
                for name in (
                    "play", "pause", "resume", "seek_forward", "seek_forward_play", "seek_back",
                    "seek_back_play", "rate_1_25", "duration_remainder",
                )
            ],
            "video_size_expectations": {
                "enabled": True,
                "complete": True,
                "semantics": "ordered_decoded_size_subsequence",
                "minimum_distinct_display_timestamps_per_size": 3,
                "entries": entries,
            },
            "playback_timeline": {
                "sample_interval_seconds": 10,
                "maximum_samples": 96,
                "truncated": False,
                "samples": [
                    {
                        "elapsed_seconds": float(index) + 10.0,
                        "observed_monotonic_seconds": float(index) + 90.0,
                        "playhead_seconds": timeline_playhead,
                        "loaded_end_seconds": timeline_end,
                        "buffered_ahead_seconds": timeline_end - timeline_playhead,
                        "decoded_width": 640,
                        "decoded_height": 360,
                        "time_control_status": "playing",
                        "indicated_bitrate": 1_200_000.0,
                    }
                    for index in timeline_offsets
                ],
            },
            "recovery_observation": {
                "observed_monotonic_seconds": 202.0,
                "player_state": {
                    "automatically_waits_to_minimize_stalling": True,
                    "playhead_seconds": 100.0,
                    "preferred_forward_buffer_duration_seconds": 0.0,
                    "loaded_time_ranges": {
                        "observations": [{"start_seconds": 0.0, "duration_seconds": 120.0, "end_seconds": 120.0}],
                        "source_count": 1, "valid_count": 1, "invalid_count": 0, "truncated": False,
                    },
                },
            },
            "steady_play_end_observation": None,
        },
        "fault_timeline": {
            "enabled": True,
            "bad_high_status_code": 503,
            "decoder_ready_monotonic_seconds": 90.0,
            "baseline_start_monotonic_seconds": 94.0,
            "bad_high_before_monotonic_seconds": 100.0,
            "bad_high_after_monotonic_seconds": 101.0,
            "recovery_before_monotonic_seconds": 200.0,
            "recovery_after_monotonic_seconds": 201.0,
        },
        "requests": {
            "media_payload_by_phase_and_representation": [[1, 0, 0], [0, 1, 0], [1, 0, 0]],
            "count_by_phase_and_representation": [[1, 0, 0], [1, 1, 0], [1, 0, 0]],
            "successful_by_representation": [1, 1, 0],
            "high_fault_status_count": 1,
            "first_recovery_high_range_request": {
                "elapsed_ms": 111_200,
                "monotonic_seconds": 201.2,
                "range_start": 0,
                "range_length": 65_536,
                "status_code": 206,
                "media_window": None,
            },
            "first_recovery_high_media_request": {
                "elapsed_ms": 111_500,
                "monotonic_seconds": 201.5,
                "range_start": 65_536,
                "range_length": 65_536,
                "status_code": 206,
                "media_window": {
                    "start_seconds": 100.0,
                    "end_seconds": 110.0,
                    "first_fragment_index": 1,
                    "last_fragment_index": 1,
                },
            },
        },
    }


class MediaNetworkAcceptanceTests(unittest.TestCase):
    def test_three_required_case_profiles_and_long_runner_budget(self) -> None:
        self.assertEqual(len(media.EXPECTED_TESTS), 3)
        self.assertEqual(set(media.EXPECTED_CASES), set(media.EXPECTED_TESTS))
        abr = media.EXPECTED_CASES[media.EXPECTED_TESTS[0]]
        self.assertEqual(
            (abr["duration"], abr["probe_duration"], abr["probe_deadline"], abr["diagnostic_only"]),
            (3600, 600, 720, False),
        )
        buffered = media.EXPECTED_CASES[media.EXPECTED_TESTS[2]]
        self.assertEqual((buffered["duration"], buffered["probe_duration"], buffered["probe_deadline"]), (160, 130, 175))
        transcoding = media.EXPECTED_CASES[media.EXPECTED_TESTS[1]]
        self.assertEqual((transcoding["duration"], transcoding["probe_duration"], transcoding["probe_deadline"]), (45, 30, 45))
        self.assertEqual(
            (media.COMPILE_BUDGET_SECONDS, media.RUNTIME_BUDGET_SECONDS,
             media.FINALIZE_BUDGET_SECONDS, media.TOTAL_BUDGET_SECONDS),
            (25 * 60, 24 * 60, 60, 50 * 60),
        )

    def test_unsigned_execution_requires_github_actions(self) -> None:
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "false"}):
            with self.assertRaisesRegex(media.MediaNetworkError, "restricted to GitHub Actions"):
                media.run(ci_unsigned=True)

    def test_explicit_unsigned_ci_keeps_original_compile_and_execution_route(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = root / "artifacts"
            artifacts.mkdir()
            commands: list[list[str]] = []

            def execute(argv: list[str], **kwargs: object) -> None:
                commands.append(argv)
                if argv[0] == "xcrun":
                    Path(argv[-1]).touch()

            with (
                mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "TMPDIR": directory}),
                mock.patch.object(media, "_require_tools"),
                mock.patch.object(media, "_run_command", side_effect=execute),
                mock.patch.object(media, "_mute_system_audio"),
                mock.patch.object(media, "validate_cargo_log"),
                mock.patch.object(media, "validate_report"),
                mock.patch.object(media, "_validate_reports_and_derive_diagnostics", return_value={"reports": []}),
                mock.patch.object(media, "_write_summary"),
                mock.patch.object(media, "_artifact_size", return_value=0),
                mock.patch.object(media, "_require_artifact_free_space") as free_space,
                mock.patch.object(media, "_run_local_signed") as signed,
            ):
                self.assertEqual(media.run(str(artifacts), ci_unsigned=True), 0)
            free_space.assert_called_once_with(artifacts.resolve())
            self.assertEqual(commands[0], media.build_compile_argv(Path(commands[0][-1])))
            self.assertEqual(commands[1], media.build_cargo_compile_argv())
            self.assertEqual(commands[2], media.build_cargo_argv())
            signed.assert_not_called()

    def test_environment_allowlist_drops_credentials_and_proxy_configuration(self) -> None:
        clean = media.clean_environment({
            "PATH": "/usr/bin", "HOME": "/tmp/home", "GITHUB_TOKEN": SYNTHETIC_ACCESS_TOKEN,
            "BILIBILI_SESSDATA": SYNTHETIC_ACCESS_TOKEN, "HTTPS_PROXY": "http://proxy",
            "AWS_ACCESS_KEY_ID": SYNTHETIC_API_KEY,
        })
        self.assertEqual(clean, {"PATH": "/usr/bin", "HOME": "/tmp/home"})

    def test_command_lines_match_native_toolchain_contract(self) -> None:
        compile_argv = media.build_compile_argv(Path("/private/probe"))
        self.assertEqual(compile_argv[:7], [
            "xcrun", "swiftc", "-swift-version", "6", "-strict-concurrency=complete",
            "-warnings-as-errors", "-parse-as-library",
        ])
        self.assertEqual(compile_argv[-2:], ["-o", "/private/probe"])
        self.assertEqual(media.build_cargo_argv(), [
            "cargo", "test", "--manifest-path", str(media.ROOT / "Cargo.toml"), "--package",
            "tvos-net-player-cache-server", "--release", "--locked", "--lib",
            "media_acceptance_tests::", "--", "--ignored", "--nocapture", "--test-threads=1",
            "--skip", media.EXTENDED_DIAGNOSTIC_TEST,
            "--color=never",
        ])
        self.assertEqual(media.build_cargo_compile_argv(), [
            "cargo", "test", "--manifest-path", str(media.ROOT / "Cargo.toml"), "--package",
            "tvos-net-player-cache-server", "--release", "--locked", "--lib", "--no-run",
            "--color=never",
        ])
        self.assertEqual(media.build_signed_test_argv(Path("/signed/media-tests")), [
            "/signed/media-tests", "media_acceptance_tests::", "--ignored", "--nocapture",
            "--test-threads=1", "--skip", media.EXTENDED_DIAGNOSTIC_TEST, "--color=never",
        ])

    def test_default_runner_keeps_exact_three_required_cases_and_skips_diagnostic(self) -> None:
        self.assertEqual(len(media.EXPECTED_TESTS), 3)
        self.assertNotIn(media.EXTENDED_DIAGNOSTIC_TEST, media.EXPECTED_TESTS)
        for argv in (media.build_cargo_argv(), media.build_signed_test_argv(Path("/signed/media-tests"))):
            self.assertEqual(argv.count("--skip"), 1)
            self.assertIn(media.EXTENDED_DIAGNOSTIC_TEST, argv)

    def _signed_receipt(self, probe: Path, tests: Path) -> dict[str, object]:
        product = {
            "identifier": "dev.example.signed", "team_id": "TESTTEAM01",
            "certificate_sha1": "A" * 40, "designated_requirement": "anchor apple generic",
            "cdhash": "B" * 40,
        }
        return {"products": {
            "probe": {**product, "path": str(probe)},
            "media-tests": {**product, "path": str(tests)},
        }}

    def test_signed_paths_are_fixed_across_runs_and_lock_covers_execution(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logs, artifacts, rust = (root / name for name in ("logs", "artifacts", "rust"))
            for path in (logs, artifacts, rust):
                path.mkdir()
            probe, tests = root / "probe", root / "media-tests"
            probe.touch()
            tests.touch()
            receipt = self._signed_receipt(probe, tests)
            active = False

            @contextmanager
            def prepared(targets: set[str], *, config_file: Path | None = None):
                nonlocal active
                self.assertEqual(targets, {"probe", "media-tests"})
                active = True
                try:
                    yield receipt
                finally:
                    active = False

            fake_build = SimpleNamespace(RUN=lambda *args, **kwargs: None, BuildError=RuntimeError,
                                         prepared_targets=prepared)
            commands: list[list[str]] = []

            def execute(argv: list[str], **kwargs: object) -> None:
                self.assertTrue(active)
                commands.append(argv)

            with (
                mock.patch.object(media, "_load_local_build", return_value=fake_build),
                mock.patch.object(media, "_mute_system_audio") as mute,
                mock.patch.object(media, "_run_command", side_effect=execute) as run_mock,
                mock.patch.object(media, "validate_cargo_log"),
                mock.patch.object(media, "validate_report"),
                mock.patch.object(media, "_validate_reports_and_derive_diagnostics", return_value={"reports": []}),
            ):
                for _ in range(2):
                    media._run_local_signed(
                        logs=logs, artifact_dir=artifacts, env={"PATH": "/usr/bin"},
                        config_file=None, total_deadline=media.time.monotonic() + 3000,
                        started=media.time.monotonic(), rust_artifacts=rust,
                    )
            self.assertEqual(commands, [media.build_signed_test_argv(tests)] * 2)
            self.assertEqual(
                [call.kwargs.get("cargo_artifact_monitor") for call in run_mock.call_args_list],
                [True, True],
            )
            self.assertEqual(run_mock.call_count, 2)
            self.assertEqual(commands[0][0], commands[1][0])
            self.assertEqual(mute.call_count, 2)
            self.assertFalse(active)

    def test_signed_preflight_failure_precedes_mute_and_playback(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logs, artifacts, rust = (root / name for name in ("logs", "artifacts", "rust"))
            for path in (logs, artifacts, rust):
                path.mkdir()
            @contextmanager
            def prepared(*args: object, **kwargs: object):
                yield {"products": {}}
            fake_build = SimpleNamespace(RUN=lambda *args, **kwargs: None, BuildError=RuntimeError,
                                         prepared_targets=prepared)
            with (
                mock.patch.object(media, "_load_local_build", return_value=fake_build),
                mock.patch.object(media, "_mute_system_audio") as mute,
                mock.patch.object(media, "_run_command") as execute,
                self.assertRaisesRegex(media.MediaNetworkError, "verified fixed probe"),
            ):
                media._run_local_signed(
                    logs=logs, artifact_dir=artifacts, env={"PATH": "/usr/bin"},
                    config_file=None, total_deadline=media.time.monotonic() + 3000,
                    started=media.time.monotonic(), rust_artifacts=rust,
                )
            mute.assert_not_called()
            execute.assert_not_called()

    def test_signed_compiler_timeout_is_bounded_and_releases_profile_context(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logs, artifacts, rust = (root / name for name in ("logs", "artifacts", "rust"))
            for path in (logs, artifacts, rust):
                path.mkdir()
            state = {"active": False}

            @contextmanager
            def prepared(*args: object, **kwargs: object):
                state["active"] = True
                try:
                    fake_build.RUN(
                        ["swiftc"], env={"PATH": "/usr/bin", "GITHUB_TOKEN": "must-drop"},
                        cwd=str(root),
                    )
                    yield self._signed_receipt(root / "probe", root / "tests")
                finally:
                    state["active"] = False

            fake_build = SimpleNamespace(RUN=lambda *args, **kwargs: None, BuildError=RuntimeError,
                                         prepared_targets=prepared)
            with (
                mock.patch.object(media, "_load_local_build", return_value=fake_build),
                mock.patch.object(media.ui, "run_process", return_value=media.ui.CommandResult(0, 0, timed_out=True)) as run_process,
                mock.patch.object(media, "_mute_system_audio") as mute,
                self.assertRaisesRegex(media.MediaNetworkError, "shared deadline"),
            ):
                media._run_local_signed(
                    logs=logs, artifact_dir=artifacts, env={"PATH": "/usr/bin"},
                    config_file=None, total_deadline=media.time.monotonic() + 3000,
                    started=media.time.monotonic(), rust_artifacts=rust,
                )
            self.assertFalse(state["active"])
            mute.assert_not_called()
            self.assertLessEqual(run_process.call_args.kwargs["max_output_bytes"], media.COMMAND_LOG_BYTES)
            self.assertEqual(run_process.call_args.kwargs["env"], {"PATH": "/usr/bin"})
            self.assertEqual(run_process.call_args.kwargs["cwd"], root)

    def test_compile_phases_share_a_deadline_and_preserve_runtime_budget(self) -> None:
        deadline = media.resolve_compile_deadline(started_at=1000, total_deadline=4000)
        self.assertEqual(deadline, 4000 - media.RUNTIME_BUDGET_SECONDS - media.FINALIZE_BUDGET_SECONDS)
        self.assertEqual(deadline, 2500)
        capped = media.resolve_compile_deadline(started_at=1000, total_deadline=5000)
        self.assertEqual(capped, 1000 + media.COMPILE_BUDGET_SECONDS)
        self.assertEqual(media.remaining_work(4000, now=2500) - media.FINALIZE_BUDGET_SECONDS,
                         media.RUNTIME_BUDGET_SECONDS)

    def test_cargo_log_accepts_captured_nocapture_records(self) -> None:
        captured = "\n".join((
            "running 3 tests",
            f"test {media.EXPECTED_TESTS[0]} ... HLS foreground range upstream returned 503 Service Unavailable",
            "ok",
            f"test {media.EXPECTED_TESTS[2]} ... HLS foreground range upstream returned 503 Service Unavailable",
            "ok",
            f"test {media.EXPECTED_TESTS[1]} ... ok",
            "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 1263 filtered out; finished in 834.52s",
            "",
        ))
        mixed = "\n".join((
            f"test {media.EXPECTED_TESTS[0]} ... ok",
            f"test {media.EXPECTED_TESTS[2]} ... ordinary nocapture diagnostic",
            "diagnostic continuation",
            "ok",
            f"test {media.EXPECTED_TESTS[1]} ... ok",
            "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 1263 filtered out",
        ))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cargo.log"
            for content in (captured, mixed):
                path.write_text(content, encoding="utf-8")
                media.validate_cargo_log(path)

    def test_cargo_log_rejects_incomplete_ambiguous_and_unexpected_records(self) -> None:
        passing = [f"test {name} ... ok" for name in media.EXPECTED_TESTS]
        summary = "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 1263 filtered out"
        cases = {
            "duplicate required test": "\n".join(passing + [passing[0], summary]),
            "missing required test": "\n".join(passing[:-1] + [summary]),
            "unknown test": "\n".join(passing + ["test media_acceptance_tests::extra ... ok", summary]),
            "diagnostic test header": "\n".join(passing + ["test HLS diagnostic ... ok", summary]),
            "failed test": "\n".join([f"test {media.EXPECTED_TESTS[0]} ... FAILED", *passing[1:], summary]),
            "ignored test": "\n".join([f"test {media.EXPECTED_TESTS[0]} ... ignored", *passing[1:], summary]),
            "orphan completion": "\n".join(["ok", *passing, summary]),
            "orphan failure": "\n".join(["FAILED", *passing, summary]),
            "orphan ignored status": "\n".join(["ignored", *passing, summary]),
            "unterminated header": "\n".join([f"test {media.EXPECTED_TESTS[0]} ...", *passing[1:], summary]),
            "ambiguous completion": "\n".join([f"test {media.EXPECTED_TESTS[0]} ... ok", "ok", *passing[1:], summary]),
            "malformed header": "\n".join(["test malformed header", *passing, summary]),
            "duplicate summary": "\n".join([*passing, summary, summary]),
            "nonterminal summary": "\n".join([*passing, summary, "trailing payload"]),
            "malformed summary": "\n".join([*passing, summary + "; finished in soon",]),
            "nonzero failed count": "\n".join([*passing, summary.replace("0 failed", "1 failed")]),
            "nonzero ignored count": "\n".join([*passing, summary.replace("0 ignored", "1 ignored")]),
            "zero-result fake summary": "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out",
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cargo.log"
            for label, content in cases.items():
                with self.subTest(case=label):
                    path.write_text(content, encoding="utf-8")
                    with self.assertRaises(media.MediaNetworkError):
                        media.validate_cargo_log(path)

    def test_cargo_failure_and_timeout_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cargo.log"
            path.write_text("", encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "timed out"):
                media.validate_cargo_log(path, timed_out=True)
            with self.assertRaisesRegex(media.MediaNetworkError, "exited 7"):
                media.validate_cargo_log(path, returncode=7)

    def test_bounded_command_rejects_timeout_output_overflow_and_nonzero_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logs = root / "logs"
            logs.mkdir()
            cases = (
                (media.ui.CommandResult(0, 0, timed_out=True), "timed out"),
                (media.ui.CommandResult(0, 0, output_limited=True), "output limit"),
                (media.ui.CommandResult(4, 0), "exited 4"),
            )
            for result, message in cases:
                with self.subTest(message=message), mock.patch.object(media.ui, "run_process", return_value=result):
                    with self.assertRaisesRegex(media.MediaNetworkError, message):
                        media._run_command(
                            ["tool"], log_path=logs / "command.log", env={"PATH": "/usr/bin"},
                            timeout=2, artifact_dir=root,
                        )

    def test_artifact_monitor_exception_stops_process_group_after_parent_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            read_fd, write_fd = os.pipe()
            process = mock.Mock()
            process.stdout = os.fdopen(read_fd, "rb", buffering=0)
            process.poll.side_effect = [None, 0]
            try:
                with (
                    mock.patch.object(media.subprocess, "Popen", return_value=process),
                    mock.patch.object(media, "_artifact_size", side_effect=media.MediaNetworkError("monitor failed")),
                    mock.patch.object(media.ui, "stop_process_group") as stop_process_group,
                ):
                    with self.assertRaisesRegex(media.MediaNetworkError, "monitor failed"):
                        media._run_command(
                            ["cargo", "test"], log_path=root / "cargo-compile.log", env={"PATH": "/usr/bin"},
                            timeout=2, artifact_dir=root, cargo_artifact_monitor=True,
                        )
                stop_process_group.assert_called_once_with(process)
            finally:
                os.close(write_fd)

    def test_absolute_job_deadline_is_validated_and_caps_runner_budget(self) -> None:
        deadline = media.resolve_work_deadline("1700", started_at=1000, now=1100)
        self.assertEqual(deadline, 1700)
        capped = media.resolve_work_deadline("1200", started_at=1000, now=1100)
        self.assertEqual(capped, 1200)
        self.assertEqual(media.remaining_work(capped, now=1150), 50)
        with self.assertRaisesRegex(media.MediaNetworkError, "invalid"):
            media.resolve_work_deadline("not-a-number", started_at=1000, now=1100)
        with self.assertRaisesRegex(media.MediaNetworkError, "invalid"):
            media.resolve_work_deadline("nan", started_at=1000, now=1100)
        with self.assertRaisesRegex(media.MediaNetworkError, "expired"):
            media.resolve_work_deadline("1099", started_at=1000, now=1100)
        with self.assertRaisesRegex(media.MediaNetworkError, "expired"):
            media.remaining_work(1100, now=1100)

    def test_missing_failed_and_truncated_reports_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            with self.assertRaisesRegex(media.MediaNetworkError, "missing"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)
            path.write_text(json.dumps({"schema_version": 1, "status": "failed"}), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "passed"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)
            path.write_text('{"schema_version": 1,', encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "truncated"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)
            path.write_text('{"schema_version":1,"schema_version":1}', encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "invalid"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)

    def test_report_admission_accepts_64k_envelope_and_rejects_overflow(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            payload["bounded_extra"] = "x" * (40 * 1024)
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertLessEqual(path.stat().st_size, 64 * 1024)
            media.validate_report(path, media.EXPECTED_TESTS[0])
            payload["bounded_extra"] = "x" * (65 * 1024)
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "oversized"):
                media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_probe_report_requires_decoded_ordered_three_frame_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            payload["case_full_name"] = media.EXPECTED_TESTS[0]
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)
            probe = payload["probe"]
            assert isinstance(probe, dict)
            expectations = probe["video_size_expectations"]
            assert isinstance(expectations, dict)
            expectations["entries"][1]["samples"][1]["display_time_seconds"] = 1.0
            for index, sample in enumerate(expectations["entries"][1]["samples"], start=4):
                sample["observed_elapsed_seconds"] = float(index) + 0.25
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "distinct display timestamps"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)

    def test_playback_timeline_admission_is_strict_and_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0])
            mutations = (
                (lambda t: t.update(truncated=True), "truncated"),
                (lambda t: t.update(maximum_samples=True), "bounds"),
                (lambda t: t["samples"].extend(dict(t["samples"][-1]) for _ in range(85)), "sample count"),
                (lambda t: t["samples"][0].update(elapsed_seconds=float("inf")), "invalid timing"),
                (lambda t: t["samples"][1].update(observed_monotonic_seconds=90.5), "clocks"),
                (lambda t: t["samples"][0].update(decoded_width=None), "decoded-size pair"),
                (lambda t: t["samples"][0].update(playhead_seconds=None), "null buffer"),
                (lambda t: t["samples"][0].update(buffered_ahead_seconds=19.0), "buffer values"),
                (lambda t: t["samples"][0].update(loaded_end_seconds=3601.0), "buffer values"),
                (lambda t: t["samples"][0].update(loaded_end_seconds=99.0), "buffer values"),
                (lambda t: t["samples"][0].update(time_control_status="unknown"), "time-control"),
            )
            for mutate, message in mutations:
                malformed = json.loads(json.dumps(payload))
                mutate(malformed["probe"]["playback_timeline"])
                path.write_text(json.dumps(malformed, allow_nan=True), encoding="utf-8")
                with self.subTest(message=message), self.assertRaisesRegex(media.MediaNetworkError, message):
                    media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_progressive_recovery_requests_validate_media_windows(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0])
            payload["requests"]["first_recovery_high_range_request"]["status_code"] = 503
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0])
            for field, key, value, message in (
                ("first_recovery_high_range_request", "status_code", True, "invalid"),
                ("first_recovery_high_media_request", "monotonic_seconds", float("nan"), "invalid"),
                ("first_recovery_high_media_request", "range_length", 0, "invalid"),
                ("first_recovery_high_media_request", "status_code", 503, "status"),
                ("first_recovery_high_range_request", "range_start", 2**64 - 1, "invalid"),
                ("first_recovery_high_range_request", "elapsed_ms", 2**64, "invalid"),
            ):
                malformed = json.loads(json.dumps(payload))
                malformed["requests"][field][key] = value
                path.write_text(json.dumps(malformed, allow_nan=True), encoding="utf-8")
                with self.subTest(field=field, key=key), self.assertRaisesRegex(media.MediaNetworkError, message):
                    media.validate_report(path, media.EXPECTED_TESTS[0])
            malformed = json.loads(json.dumps(payload))
            malformed["requests"]["first_recovery_high_media_request"]["range_start"] = 2**64 - 1
            malformed["requests"]["first_recovery_high_media_request"]["range_length"] = 2
            path.write_text(json.dumps(malformed), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "invalid first_recovery_high_media_request fields"):
                media.validate_report(path, media.EXPECTED_TESTS[0])
            malformed = json.loads(json.dumps(payload))
            malformed["requests"]["first_recovery_high_media_request"]["range_start"] = 2**64 - 1
            malformed["requests"]["first_recovery_high_media_request"]["range_length"] = 1
            path.write_text(json.dumps(malformed), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "invalid first_recovery_high_media_request fields"):
                media.validate_report(path, media.EXPECTED_TESTS[0])
            malformed = json.loads(json.dumps(payload))
            malformed["requests"]["first_recovery_high_media_request"]["media_window"] = None
            path.write_text(json.dumps(malformed), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "no overlapping"):
                media.validate_report(path, media.EXPECTED_TESTS[0])
            malformed = json.loads(json.dumps(payload))
            malformed["requests"]["first_recovery_high_media_request"]["monotonic_seconds"] = 199.0
            path.write_text(json.dumps(malformed), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "precedes the recovery clock bracket"):
                media.validate_report(path, media.EXPECTED_TESTS[0])
            inside_bracket = json.loads(json.dumps(payload))
            inside_bracket["requests"]["first_recovery_high_media_request"]["monotonic_seconds"] = 200.5
            path.write_text(json.dumps(inside_bracket), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0])
            malformed = json.loads(json.dumps(payload))
            malformed["requests"]["first_recovery_high_media_request"]["media_window"].update(
                end_seconds=3601.0
            )
            path.write_text(json.dumps(malformed), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "out-of-bounds"):
                media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_probe_duration_errors_precede_playback_timeline_access(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            del payload["probe"]["overall_deadline_seconds"]
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "Probe duration or cooperative deadline"):
                media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_recovery_diagnostics_keep_bracket_and_only_use_fresh_preceding_sample(self) -> None:
        payload = report_payload(media.EXPECTED_ABR_SIZES)
        diagnostic = media.recovery_latency_diagnostics(payload)
        self.assertEqual(diagnostic["recovery_to_request_seconds"], {"lower_bound": 0.5, "upper_bound": 1.5})
        self.assertAlmostEqual(diagnostic["first_high_decoded_vs_request_seconds"], 1.5)
        self.assertEqual(diagnostic["request_media_window"]["first_fragment_index"], 1)
        sample = diagnostic["nearest_preceding_timeline_sample"]
        self.assertEqual(sample["age_seconds"], 0.0)
        self.assertEqual(sample["requested_media_overlap_seconds"], 10.0)

        stale = json.loads(json.dumps(payload))
        stale["requests"]["first_recovery_high_media_request"]["monotonic_seconds"] = 220.0
        stale_diagnostic = media.recovery_latency_diagnostics(stale)
        self.assertIsNone(stale_diagnostic["nearest_preceding_timeline_sample"])
        before_bracket = json.loads(json.dumps(payload))
        before_bracket["requests"]["first_recovery_high_media_request"]["monotonic_seconds"] = 199.0
        before_diagnostic = media.recovery_latency_diagnostics(before_bracket)
        self.assertEqual(before_diagnostic["recovery_to_request_seconds"], {"lower_bound": 0.0, "upper_bound": -1.0})
        missing = json.loads(json.dumps(payload))
        del missing["requests"]["first_recovery_high_media_request"]
        missing_diagnostic = media.recovery_latency_diagnostics(missing)
        self.assertFalse(missing_diagnostic["available"])
        self.assertIsNone(missing_diagnostic["recovery_to_request_seconds"])

    def test_derived_diagnostics_fit_existing_summary_and_preserve_unavailable_cases(self) -> None:
        abr = report_payload(media.EXPECTED_ABR_SIZES)
        transcoded = json.loads(json.dumps(abr))
        transcoded["case_full_name"] = media.EXPECTED_TESTS[1]
        buffered = json.loads(json.dumps(abr))
        buffered["case_full_name"] = media.EXPECTED_TESTS[2]
        buffered["requests"].pop("first_recovery_high_range_request")
        buffered["requests"].pop("first_recovery_high_media_request")
        with tempfile.TemporaryDirectory() as directory:
            artifact_dir = Path(directory)
            rust_dir = artifact_dir / "rust"
            rust_dir.mkdir()
            with mock.patch.object(media, "validate_report", side_effect=[buffered, abr, transcoded]):
                diagnostics = media._validate_reports_and_derive_diagnostics(rust_dir)
            media._write_summary(
                artifact_dir, status="passed", message="ok", started=media.time.monotonic(),
                recovery_diagnostics=diagnostics,
            )
            summary = json.loads((artifact_dir / "summary.json").read_text(encoding="utf-8"))
            self.assertEqual(len(summary["recovery_latency_diagnostics"]["reports"]), 3)
            self.assertLessEqual((artifact_dir / "summary.json").stat().st_size, 16 * 1024)
            self.assertIn("no causal claim", summary["recovery_latency_diagnostics"]["interpretation"])
            self.assertFalse(summary["recovery_latency_diagnostics"]["reports"][0]["available"])

    def test_abr_report_requires_fault_window_and_phase_bound_decoded_frames(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)

            payload["fault_timeline"]["bad_high_status_code"] = 200
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "503 fault window"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)

            payload = report_payload(media.EXPECTED_ABR_SIZES)
            payload["probe"]["video_size_expectations"]["entries"][1]["samples"][0][
                "observed_monotonic_seconds"
            ] = 100.5
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "high-low fault phases"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)

            payload = report_payload(media.EXPECTED_ABR_SIZES)
            payload["requests"]["media_payload_by_phase_and_representation"][2][0] = 0
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "recovered high traffic"):
                media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_buffered_case_requires_steady_end_asset_coverage_and_accepts_frame_tail(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "buffered.json"
            payload = report_payload(
                ((640, 360), (320, 180)), scenario="buffered_continuity", duration=160,
                probe_duration=130, probe_deadline=175,
            )
            payload["case_full_name"] = media.EXPECTED_TESTS[2]
            payload["requests"]["media_payload_by_phase_and_representation"][2][0] = 0
            probe = payload["probe"]
            probe["video_size_expectations"]["entries"] = probe["video_size_expectations"]["entries"][:2]
            probe["recovery_observation"]["player_state"]["playhead_seconds"] = 120.0
            probe["recovery_observation"]["player_state"]["loaded_time_ranges"]["observations"] = [
                {"start_seconds": 0.0, "duration_seconds": 150.0, "end_seconds": 150.0}
            ]
            probe["recovery_observation"]["player_state"]["loaded_time_ranges"].update(
                source_count=1, valid_count=1, invalid_count=0, truncated=False
            )
            probe["steady_play_end_observation"] = {
                "observed_monotonic_seconds": 315.0,
                "player_state": {
                    "automatically_waits_to_minimize_stalling": True,
                    "playhead_seconds": 150.0,
                    "preferred_forward_buffer_duration_seconds": 0.0,
                    "loaded_time_ranges": {
                        "observations": [{
                            "start_seconds": 0.0, "duration_seconds": 160.2, "end_seconds": 160.2,
                        }],
                        "source_count": 1, "valid_count": 1, "invalid_count": 0, "truncated": False,
                    },
                },
            }
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[2])
            payload["requests"].pop("first_recovery_high_range_request")
            payload["requests"].pop("first_recovery_high_media_request")
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[2])
            probe["steady_play_end_observation"]["player_state"]["loaded_time_ranges"]["observations"][0][
                "end_seconds"
            ] = 1000.0
            probe["steady_play_end_observation"]["player_state"]["loaded_time_ranges"]["observations"][0][
                "duration_seconds"
            ] = 1000.0
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "source duration"):
                media.validate_report(path, media.EXPECTED_TESTS[2])

    def test_future_recovery_observation_rejects_missing_truncated_and_beyond_source_ranges(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0])
            variants = []
            missing = json.loads(json.dumps(payload))
            missing["probe"]["recovery_observation"] = None
            variants.append((missing, "passive observation"))
            truncated = json.loads(json.dumps(payload))
            truncated["probe"]["recovery_observation"]["player_state"]["loaded_time_ranges"]["truncated"] = True
            variants.append((truncated, "missing or truncated"))
            beyond = json.loads(json.dumps(payload))
            ranges = beyond["probe"]["recovery_observation"]["player_state"]["loaded_time_ranges"]
            ranges["observations"][0].update(duration_seconds=3601.0, end_seconds=3601.0)
            variants.append((beyond, "source duration"))
            beyond_playhead = json.loads(json.dumps(payload))
            beyond_playhead["probe"]["recovery_observation"]["player_state"]["playhead_seconds"] = 3601.0
            variants.append((beyond_playhead, "playhead outside source duration"))
            for malformed, message in variants:
                with self.subTest(message=message):
                    path.write_text(json.dumps(malformed), encoding="utf-8")
                    with self.assertRaisesRegex(media.MediaNetworkError, message):
                        media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_scenario_and_source_duration_are_bound_to_named_case(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0])
            for field, value in (("scenario", "buffered_continuity"), ("source_duration_seconds", 160)):
                malformed = json.loads(json.dumps(payload))
                malformed[field] = value
                path.write_text(json.dumps(malformed), encoding="utf-8")
                with self.subTest(field=field), self.assertRaisesRegex(media.MediaNetworkError, "scenario or source duration"):
                    media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_playlist_preflight_requires_rfc8216_semantics_and_exact_positive_u64_bandwidth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[0])

            malformed_cases = (
                ("missing", None),
                ("missing_semantics", {"high_variant_peak_bandwidth_bits_per_second": 1_200_000,
                                        "advertised_high_bandwidth_bits_per_second": 1_200_000}),
                ("under_advertised", {"bandwidth_semantics": "rfc8216_contiguous_duration_window",
                                       "high_variant_peak_bandwidth_bits_per_second": 1_200_000,
                                       "advertised_high_bandwidth_bits_per_second": 1_199_999}),
                ("over_advertised", {"bandwidth_semantics": "rfc8216_contiguous_duration_window",
                                      "high_variant_peak_bandwidth_bits_per_second": 1_200_000,
                                      "advertised_high_bandwidth_bits_per_second": 1_200_001}),
                ("boolean", {"bandwidth_semantics": "rfc8216_contiguous_duration_window",
                             "high_variant_peak_bandwidth_bits_per_second": True,
                             "advertised_high_bandwidth_bits_per_second": True}),
                ("float", {"bandwidth_semantics": "rfc8216_contiguous_duration_window",
                           "high_variant_peak_bandwidth_bits_per_second": 1_200_000.0,
                           "advertised_high_bandwidth_bits_per_second": 1_200_000.0}),
                ("zero", {"bandwidth_semantics": "rfc8216_contiguous_duration_window",
                           "high_variant_peak_bandwidth_bits_per_second": 0,
                           "advertised_high_bandwidth_bits_per_second": 0}),
                ("overflow", {"bandwidth_semantics": "rfc8216_contiguous_duration_window",
                              "high_variant_peak_bandwidth_bits_per_second": 2**64,
                              "advertised_high_bandwidth_bits_per_second": 2**64}),
            )
            for name, preflight in malformed_cases:
                with self.subTest(name=name):
                    malformed = json.loads(json.dumps(payload))
                    if preflight is None:
                        malformed.pop("hls_playlist_preflight")
                    else:
                        malformed["hls_playlist_preflight"] = preflight
                    path.write_text(json.dumps(malformed), encoding="utf-8")
                    with self.assertRaisesRegex(media.MediaNetworkError, "preflight semantics or exact advertised bandwidth"):
                        media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_playlist_preflight_is_required_for_each_normal_case(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "native.json"
            for case_name, contract in media.EXPECTED_CASES.items():
                payload = report_payload(
                    contract["sizes"], scenario=contract["scenario"], duration=contract["duration"]
                )
                payload["case_full_name"] = case_name
                payload["probe"]["requested_duration_seconds"] = contract["probe_duration"]
                payload["probe"]["overall_deadline_seconds"] = contract["probe_deadline"]
                if case_name == media.EXPECTED_TESTS[1]:
                    payload["probe"]["video_size_expectations"]["entries"] = payload["probe"][
                        "video_size_expectations"]["entries"][:1]
                if case_name == media.EXPECTED_TESTS[2]:
                    payload["probe"]["video_size_expectations"]["entries"] = payload["probe"][
                        "video_size_expectations"]["entries"][:2]
                    payload["probe"]["recovery_observation"]["player_state"]["playhead_seconds"] = 120.0
                    payload["probe"]["recovery_observation"]["player_state"]["loaded_time_ranges"][
                        "observations"][0].update(duration_seconds=150.0, end_seconds=150.0)
                    payload["probe"]["steady_play_end_observation"] = {
                        "observed_monotonic_seconds": 315.0,
                        "player_state": {
                            "automatically_waits_to_minimize_stalling": True,
                            "playhead_seconds": 150.0,
                            "preferred_forward_buffer_duration_seconds": 0.0,
                            "loaded_time_ranges": {
                                "observations": [{
                                    "start_seconds": 0.0, "duration_seconds": 160.2, "end_seconds": 160.2,
                                }],
                                "source_count": 1, "valid_count": 1, "invalid_count": 0, "truncated": False,
                            },
                        },
                    }
                path.write_text(json.dumps(payload), encoding="utf-8")
                media.validate_report(path, case_name)
                payload.pop("hls_playlist_preflight")
                path.write_text(json.dumps(payload), encoding="utf-8")
                with self.subTest(case=case_name), self.assertRaisesRegex(
                    media.MediaNetworkError, "preflight semantics or exact advertised bandwidth"
                ):
                    media.validate_report(path, case_name)

    def test_diagnostic_only_and_unknown_diagnostic_reports_are_inadmissible(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr-extended-diagnostic.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            payload["diagnostic_only"] = True
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "diagnostic_only value is not false"):
                media.validate_report(path, media.EXPECTED_TESTS[0])
            payload["diagnostic_only"] = False
            payload["case_full_name"] = media.EXTENDED_DIAGNOSTIC_TEST
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "not in the acceptance contract"):
                media.validate_report(path, media.EXTENDED_DIAGNOSTIC_TEST)
            payload.pop("diagnostic_only")
            payload["case_full_name"] = media.EXPECTED_TESTS[0]
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "diagnostic_only value is not false"):
                media.validate_report(path, media.EXPECTED_TESTS[0])

    def test_artifact_volume_preflight_requires_six_gibibytes_without_exposing_capacity(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_dir = Path(directory)
            with mock.patch.object(
                media.shutil, "disk_usage",
                return_value=SimpleNamespace(free=media.MIN_ARTIFACT_FREE_BYTES - 1),
            ):
                with self.assertRaisesRegex(media.MediaNetworkError, "insufficient free space") as caught:
                    media._require_artifact_free_space(artifact_dir)
            self.assertNotIn(str(media.MIN_ARTIFACT_FREE_BYTES - 1), str(caught.exception))
            with mock.patch.object(
                media.shutil, "disk_usage",
                return_value=SimpleNamespace(free=media.MIN_ARTIFACT_FREE_BYTES),
            ):
                media._require_artifact_free_space(artifact_dir)

    def test_report_versions_and_required_counts_must_be_exact_integers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            for value in (True, 1.0):
                for field in ("schema_version", "probe.schema_version"):
                    with self.subTest(field=field, value=value):
                        payload = report_payload(media.EXPECTED_ABR_SIZES)
                        payload["case_full_name"] = media.EXPECTED_TESTS[0]
                        target = payload
                        if field.startswith("probe."):
                            target = payload["probe"]
                            field = "schema_version"
                        target[field] = value
                        path.write_text(json.dumps(payload), encoding="utf-8")
                        with self.assertRaisesRegex(media.MediaNetworkError, "version-1"):
                            media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)
            for field in ("minimum", "required"):
                for value in (True, 3.0):
                    with self.subTest(field=field, value=value):
                        payload = report_payload(media.EXPECTED_ABR_SIZES)
                        payload["case_full_name"] = media.EXPECTED_TESTS[0]
                        expectations = payload["probe"]["video_size_expectations"]
                        if field == "minimum":
                            expectations["minimum_distinct_display_timestamps_per_size"] = value
                            error = "three distinct timestamps"
                        else:
                            expectations["entries"][0]["required_distinct_frame_count"] = value
                            error = "three-frame requirement"
                        path.write_text(json.dumps(payload), encoding="utf-8")
                        with self.assertRaisesRegex(media.MediaNetworkError, error):
                            media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)

    def test_observed_elapsed_time_must_be_nondecreasing_across_entries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "abr.json"
            payload = report_payload(media.EXPECTED_ABR_SIZES)
            payload["case_full_name"] = media.EXPECTED_TESTS[0]
            payload["probe"]["video_size_expectations"]["entries"][1]["samples"][0]["observed_elapsed_seconds"] = 3.0
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "elapsed timing is out of order"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)

    def test_each_report_is_bound_to_its_case_and_expected_dimensions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "transcoding.json"
            payload = report_payload(
                media.EXPECTED_TRANSCODE_SIZES, scenario="transcoded_output", duration=45
            )
            payload["case_full_name"] = media.EXPECTED_TESTS[1]
            payload["probe"]["requested_duration_seconds"] = 30
            payload["probe"]["overall_deadline_seconds"] = 45
            path.write_text(json.dumps(payload), encoding="utf-8")
            media.validate_report(path, media.EXPECTED_TESTS[1], media.EXPECTED_TRANSCODE_SIZES)
            payload["probe"]["video_size_expectations"]["entries"][0]["expected_width"] += 1
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "unexpected ordered dimension"):
                media.validate_report(path, media.EXPECTED_TESTS[1], media.EXPECTED_TRANSCODE_SIZES)

    def test_artifact_tree_rejects_aggregate_overflow_and_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "payload.bin").write_bytes(b"123456")
            with self.assertRaisesRegex(media.MediaNetworkError, "aggregate cap"):
                media._artifact_size(root, limit=5)
            (root / "payload.bin").unlink()
            (root / "link").symlink_to(root)
            with self.assertRaisesRegex(media.MediaNetworkError, "symlink"):
                media._artifact_size(root)

    def test_explicit_artifact_directory_accepts_empty_and_rejects_nonempty_or_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "artifacts"
            path.mkdir()
            self.assertEqual(media._fresh_artifact_dir(str(path)), path.resolve())
            (path / "existing.log").write_text("retained", encoding="utf-8")
            with self.assertRaisesRegex(media.MediaNetworkError, "empty"):
                media._fresh_artifact_dir(str(path))
            link = Path(directory) / "artifact-link"
            link.symlink_to(path)
            with self.assertRaisesRegex(media.MediaNetworkError, "symlink"):
                media._fresh_artifact_dir(str(link))


if __name__ == "__main__":
    unittest.main()
