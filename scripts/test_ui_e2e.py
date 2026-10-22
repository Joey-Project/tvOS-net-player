from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("ui-e2e.py")
SPEC = importlib.util.spec_from_file_location("ui_e2e", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
ui_e2e = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ui_e2e
SPEC.loader.exec_module(ui_e2e)


class ProcessSupervisorTests(unittest.TestCase):
    def test_deadline_terminates_and_reaps_owned_process(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log_path = Path(directory) / "deadline.log"
            result = ui_e2e.run_process(
                [sys.executable, "-c", "import time; print('started', flush=True); time.sleep(10)"],
                log_path=log_path,
                timeout_seconds=0.2,
                max_output_bytes=1024,
            )
            self.assertTrue(result.timed_out)
            self.assertLessEqual(result.bytes_written, 1024)
            self.assertIn("started", log_path.read_text())

    def test_output_limit_terminates_owned_process(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log_path = Path(directory) / "output.log"
            result = ui_e2e.run_process(
                [sys.executable, "-c", "print('x' * 8192)"],
                log_path=log_path,
                timeout_seconds=5,
                max_output_bytes=128,
            )
            self.assertTrue(result.output_limited)
            self.assertLessEqual(result.bytes_written, 128)
            self.assertLessEqual(log_path.stat().st_size, 128)


class CleanupTests(unittest.TestCase):
    def test_cleanup_runs_all_callbacks_and_preserves_primary_error(self) -> None:
        order: list[str] = []
        cleanup = ui_e2e.CleanupStack()
        cleanup.defer("first", lambda: order.append("first"))

        def fail_cleanup() -> None:
            order.append("failing")
            raise RuntimeError("cleanup failure")

        cleanup.defer("failing", fail_cleanup)
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(ValueError, "primary"):
                with cleanup:
                    raise ValueError("primary")
        self.assertEqual(order, ["failing", "first"])
        self.assertEqual(len(cleanup.failures), 1)

    def test_failure_is_logged_and_staged_before_private_workspace_removal(self) -> None:
        for fail in (False, True):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                work_dir = root / "private-work"
                logs_dir = work_dir / "logs"
                logs_dir.mkdir(parents=True)
                runner_log = logs_dir / "runner.log"
                runner_log.write_text("started\n", encoding="utf-8")
                artifact_dir = root / "artifacts"
                stager = ui_e2e.ArtifactStager(artifact_dir)
                cleanup_order: list[str] = []

                def record_failure(error: BaseException) -> None:
                    with runner_log.open("a", encoding="utf-8") as output:
                        output.write(f"failed: {error}\n")

                cleanup = ui_e2e.CleanupStack(on_failure=record_failure)
                result_bundle = work_dir / "UIE2E.xcresult"
                ui_e2e.defer_workspace_cleanup(
                    cleanup,
                    stager,
                    work_dir=work_dir,
                    result_bundle=result_bundle,
                )
                cleanup.defer("server", lambda: cleanup_order.append("server"))
                cleanup.defer("simulator", lambda: cleanup_order.append("simulator"))

                if fail:
                    with self.assertRaisesRegex(RuntimeError, "test failure"):
                        with cleanup:
                            raise RuntimeError("test failure")
                else:
                    with cleanup:
                        pass

                self.assertEqual(cleanup_order, ["simulator", "server"])
                self.assertFalse(work_dir.exists())
                staged_log = (artifact_dir / "runner.log").read_text(encoding="utf-8")
                self.assertIn("started", staged_log)
                if fail:
                    self.assertIn("failed: test failure", staged_log)


class RunnerContractTests(unittest.TestCase):
    def test_missing_or_non_loopback_endpoint_fails_closed(self) -> None:
        for address in (None, "", "http://localhost:5000", "http://127.0.0.1:70000"):
            with self.subTest(address=address), self.assertRaises(ui_e2e.UIE2EError):
                ui_e2e.validate_server_address(address)

    def test_xcodebuild_uses_unique_bundle_id_and_endpoint_build_setting(self) -> None:
        bundle_id = "dev.joey.tvos-net-player.uie2e." + "a" * 32
        command = ui_e2e.build_xcodebuild_command(
            platform="macos",
            server_address="http://127.0.0.1:43210",
            app_bundle_identifier=bundle_id,
            derived_data_path=Path("/tmp/DerivedData"),
            result_bundle_path=Path("/tmp/UIE2E.xcresult"),
            destination="platform=macOS",
        )
        self.assertIn("-quiet", command)
        self.assertIn(f"APP_BUNDLE_IDENTIFIER={bundle_id}", command)
        self.assertIn("UI_E2E_SERVER_ADDRESS=http://127.0.0.1:43210", command)
        self.assertEqual(
            sum(argument.startswith("APP_BUNDLE_IDENTIFIER=") for argument in command),
            1,
        )

    def test_scheme_requires_test_action_endpoint_mapping(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scheme = Path(directory) / "UIE2E.xcscheme"
            scheme.write_text(
                '<Scheme><TestAction shouldUseLaunchSchemeArgsEnv="NO">'
                '<EnvironmentVariables><EnvironmentVariable '
                'key="UI_E2E_SERVER_ADDRESS" '
                'value="$(UI_E2E_SERVER_ADDRESS)" isEnabled="YES" />'
                '</EnvironmentVariables><Testables><TestableReference skipped="NO">'
                '<BuildableReference BlueprintName="ExampleUITests" />'
                '</TestableReference></Testables></TestAction></Scheme>',
                encoding="utf-8",
            )
            ui_e2e.verify_scheme_contract(scheme, "ExampleUITests")
            scheme.write_text(
                '<Scheme><TestAction shouldUseLaunchSchemeArgsEnv="NO">'
                '<Testables><TestableReference><BuildableReference '
                'BlueprintName="ExampleUITests" /></TestableReference>'
                '</Testables></TestAction></Scheme>',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "explicitly map"):
                ui_e2e.verify_scheme_contract(scheme, "ExampleUITests")
            scheme.write_text(
                '<Scheme><TestAction shouldUseLaunchSchemeArgsEnv="NO">'
                '<EnvironmentVariables><EnvironmentVariable '
                'key="UI_E2E_SERVER_ADDRESS" '
                'value="$(UI_E2E_SERVER_ADDRESS)" isEnabled="YES" />'
                '</EnvironmentVariables><Testables><TestableReference skipped="YES">'
                '<BuildableReference BlueprintName="ExampleUITests" />'
                '</TestableReference></Testables></TestAction></Scheme>',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "does not test"):
                ui_e2e.verify_scheme_contract(scheme, "ExampleUITests")

    def test_server_cli_uses_alternating_pairs_and_disables_optional_services(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            argv = ui_e2e.build_server_argv(
                Path("/server"),
                cache_dir=root / "Cache",
                state_dir=root / "State",
                grpc_port=41001,
                media_port=41002,
            )
        args = argv[1:]
        self.assertEqual(len(args) % 2, 0)
        options = dict(zip(args[::2], args[1::2], strict=True))
        self.assertTrue(all(key.startswith("--") for key in args[::2]))
        self.assertFalse(any("=" in key for key in args[::2]))
        self.assertEqual(options["--Cache:ServerName"], "UI E2E Cache")
        self.assertEqual(options["--Cache:BonjourEnabled"], "false")
        self.assertEqual(options["--Cache:AllowBilibiliLoginSessions"], "false")
        self.assertEqual(options["--Cache:BilibiliWorkerEnabled"], "false")
        self.assertFalse(any("credential" in arg.lower() for arg in args))

    def test_xcresulttool_commands_use_supported_default_json_output(self) -> None:
        for kind in ("summary", "tests"):
            command = ui_e2e.build_xcresulttool_command(kind, Path("/tmp/test.xcresult"))
            self.assertEqual(command[-2:], ["--path", "/tmp/test.xcresult"])
            self.assertNotIn("--format", command)

    def test_artifact_allowlist_and_aggregate_cap(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logs = root / "logs"
            logs.mkdir()
            runner_log = logs / "runner.log"
            runner_log.write_bytes(b"12345678")
            server_log = logs / "server.log"
            server_log.write_bytes(b"abcdefgh")
            stager = ui_e2e.ArtifactStager(root / "artifacts", max_bytes=12)
            stager.stage_file(runner_log)
            stager.stage_file(server_log)
            self.assertEqual(stager.used_bytes, 8)
            self.assertTrue((root / "artifacts" / "runner.log").is_file())
            self.assertFalse((root / "artifacts" / "server.log").exists())

            unapproved = logs / "credentials.json"
            unapproved.write_text("never stage", encoding="utf-8")
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "not allowlisted"):
                stager.stage_file(unapproved)


class XcresultValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.summary = {
            "totalTestCount": 1,
            "passedTests": 1,
            "failedTests": 0,
            "skippedTests": 0,
        }
        self.tests = {
            "testNodes": [
                {
                    "nodeType": "Test Target",
                    "name": "MacOSNetPlayerUITests",
                    "children": [
                        {"nodeType": "Test Case", "name": "testPlayback", "result": "Passed"}
                    ],
                }
            ]
        }

    def test_requires_exact_target_and_executed_pass(self) -> None:
        counts = ui_e2e.validate_xcresult(
            self.summary, self.tests, "MacOSNetPlayerUITests"
        )
        self.assertEqual(counts["target_passed"], 1)
        wrong_target = json.loads(json.dumps(self.tests))
        wrong_target["testNodes"][0]["name"] = "MacOSNetPlayerUITestsExtra"
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "does not contain expected"):
            ui_e2e.validate_xcresult(
                self.summary, wrong_target, "MacOSNetPlayerUITests"
            )

    def test_rejects_zero_tests_skipped_tests_and_missing_target(self) -> None:
        zero = dict(self.summary, totalTestCount=0, passedTests=0)
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "no UI test"):
            ui_e2e.validate_xcresult(zero, self.tests, "MacOSNetPlayerUITests")

        skipped_summary = dict(self.summary, totalTestCount=2, passedTests=1, skippedTests=1)
        skipped_tree = json.loads(json.dumps(self.tests))
        skipped_tree["testNodes"][0]["children"].append(
            {"nodeType": "Test Case", "name": "testSkipped", "result": "Skipped"}
        )
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "skipped UI tests"):
            ui_e2e.validate_xcresult(
                skipped_summary, skipped_tree, "MacOSNetPlayerUITests"
            )

        with self.assertRaisesRegex(ui_e2e.UIE2EError, "does not contain expected"):
            ui_e2e.validate_xcresult(self.summary, {"testNodes": []}, "MacOSNetPlayerUITests")


if __name__ == "__main__":
    unittest.main()
