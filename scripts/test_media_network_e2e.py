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


def report_payload(sizes: tuple[tuple[int, int], ...]) -> dict[str, object]:
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
        "probe": {
            "schema_version": 1,
            "status": "completed",
            "requested_duration_seconds": 130,
            "overall_deadline_seconds": 175,
            "deadline_mode": "cooperative",
            "decoded_frame_count": 9,
            "audio_track_present": True,
            "enabled_audio_track_count": 1,
            "steps": [
                {"name": name, "status": "completed"}
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
    }


class MediaNetworkAcceptanceTests(unittest.TestCase):
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
                mock.patch.object(media, "_write_summary"),
                mock.patch.object(media, "_artifact_size", return_value=0),
                mock.patch.object(media, "_run_local_signed") as signed,
            ):
                self.assertEqual(media.run(str(artifacts), ci_unsigned=True), 0)
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
            "--color=never",
        ])
        self.assertEqual(media.build_cargo_compile_argv(), [
            "cargo", "test", "--manifest-path", str(media.ROOT / "Cargo.toml"), "--package",
            "tvos-net-player-cache-server", "--release", "--locked", "--lib", "--no-run",
            "--color=never",
        ])
        self.assertEqual(media.build_signed_test_argv(Path("/signed/media-tests")), [
            "/signed/media-tests", "media_acceptance_tests::", "--ignored", "--nocapture",
            "--test-threads=1", "--color=never",
        ])

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
            ):
                for _ in range(2):
                    media._run_local_signed(
                        logs=logs, artifact_dir=artifacts, env={"PATH": "/usr/bin"},
                        config_file=None, total_deadline=media.time.monotonic() + 600,
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
                    config_file=None, total_deadline=media.time.monotonic() + 600,
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
                    config_file=None, total_deadline=media.time.monotonic() + 600,
                    started=media.time.monotonic(), rust_artifacts=rust,
                )
            self.assertFalse(state["active"])
            mute.assert_not_called()
            self.assertLessEqual(run_process.call_args.kwargs["max_output_bytes"], media.COMMAND_LOG_BYTES)
            self.assertEqual(run_process.call_args.kwargs["env"], {"PATH": "/usr/bin"})
            self.assertEqual(run_process.call_args.kwargs["cwd"], root)

    def test_compile_phases_share_a_deadline_and_preserve_runtime_budget(self) -> None:
        deadline = media.resolve_compile_deadline(started_at=1000, total_deadline=2100)
        self.assertEqual(deadline, 2100 - media.RUNTIME_BUDGET_SECONDS - media.FINALIZE_BUDGET_SECONDS)
        self.assertEqual(deadline, 1560)
        capped = media.resolve_compile_deadline(started_at=1000, total_deadline=5000)
        self.assertEqual(capped, 1000 + media.COMPILE_BUDGET_SECONDS)
        self.assertEqual(media.remaining_work(2100, now=1560) - media.FINALIZE_BUDGET_SECONDS,
                         media.RUNTIME_BUDGET_SECONDS)

    def test_cargo_log_requires_both_exact_tests_and_exact_counts(self) -> None:
        lines = "\n".join(f"test {name} ... ok" for name in media.EXPECTED_TESTS)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cargo.log"
            path.write_text(
                lines + "\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
                encoding="utf-8",
            )
            media.validate_cargo_log(path)
            path.write_text(
                lines + f"\ntest {media.EXPECTED_TESTS[0]} ... ok\n"
                "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(media.MediaNetworkError, "exactly one passing"):
                media.validate_cargo_log(path)
            path.write_text(
                "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(media.MediaNetworkError, "exactly one passing"):
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
            with self.assertRaisesRegex(media.MediaNetworkError, "high-low-high fault phases"):
                media.validate_report(path, media.EXPECTED_TESTS[0], media.EXPECTED_ABR_SIZES)

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
            payload = report_payload(media.EXPECTED_TRANSCODE_SIZES)
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
