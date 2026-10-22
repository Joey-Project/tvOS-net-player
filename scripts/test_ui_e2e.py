from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


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

    def test_timeout_reports_denied_cleanup_without_claiming_reaping(self) -> None:
        process = mock.Mock()
        process.pid = 4242
        process.poll.return_value = None
        process.stdout.fileno.return_value = 99
        selector = mock.Mock()
        denied = PermissionError("signal denied")
        command = ["xcrun", "simctl", "list", "runtimes", "--json"]
        with tempfile.TemporaryDirectory() as directory:
            log_path = Path(directory) / "timeout.log"
            with (
                mock.patch.object(ui_e2e.subprocess, "Popen", return_value=process),
                mock.patch.object(ui_e2e.selectors, "DefaultSelector", return_value=selector),
                mock.patch.object(ui_e2e.os, "set_blocking"),
                mock.patch.object(ui_e2e.time, "monotonic", side_effect=[0.0, 1.0]),
                mock.patch.object(ui_e2e.os, "killpg", side_effect=denied) as killpg,
                self.assertRaises(ui_e2e.UIE2EError) as caught,
            ):
                ui_e2e.run_process(
                    command,
                    log_path=log_path,
                    timeout_seconds=0.5,
                    max_output_bytes=1024,
                )
            message = str(caught.exception)
            self.assertIn("exceeded its 0.5-second deadline", message)
            self.assertIn("cleanup unverified; child reaping not confirmed", message)
            self.assertIn("PermissionError: signal denied", message)
            self.assertIn(f"argv={json.dumps(command)}", message)
            self.assertIn("pid=4242", message)
            self.assertIn("max_output_bytes=1024", message)
            self.assertIn(f"log={log_path}", message)
            self.assertIs(caught.exception.__cause__, denied)
            killpg.assert_called_once_with(4242, ui_e2e.signal.SIGTERM)
            process.wait.assert_not_called()
            process.stdout.close.assert_called_once()
            selector.close.assert_called_once()

    def test_primary_command_error_keeps_cause_when_cleanup_is_denied(self) -> None:
        process = mock.Mock()
        process.pid = 4243
        process.poll.return_value = None
        process.stdout.fileno.return_value = 99
        selector = mock.Mock()
        selector.select.return_value = [(mock.Mock(fd=99), ui_e2e.selectors.EVENT_READ)]
        primary = OSError("read failure " + "x" * 8192)
        denied = PermissionError("signal denied")
        with tempfile.TemporaryDirectory() as directory:
            log_path = Path(directory) / "read-error.log"
            with (
                mock.patch.object(ui_e2e.subprocess, "Popen", return_value=process),
                mock.patch.object(ui_e2e.selectors, "DefaultSelector", return_value=selector),
                mock.patch.object(ui_e2e.os, "set_blocking"),
                mock.patch.object(ui_e2e.time, "monotonic", return_value=0.0),
                mock.patch.object(ui_e2e.os, "read", side_effect=primary),
                mock.patch.object(ui_e2e.os, "killpg", side_effect=denied),
                self.assertRaises(ui_e2e.UIE2EError) as caught,
            ):
                ui_e2e.run_process(
                    ["fixture-command"],
                    log_path=log_path,
                    timeout_seconds=1,
                    max_output_bytes=1024,
                )
            self.assertIs(caught.exception.__cause__, primary)
            self.assertIn("Command failed: OSError: read failure", str(caught.exception))
            self.assertIn("cleanup unverified", str(caught.exception))
            self.assertIn("PermissionError: signal denied", str(caught.exception))
            self.assertLess(len(str(caught.exception)), 2048)
            process.wait.assert_not_called()
            process.stdout.close.assert_called_once()
            selector.close.assert_called_once()


class ToolInventoryTests(unittest.TestCase):
    def test_macos_inventory_never_invokes_simctl(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(ui_e2e, "run_capture", return_value="tool metadata\n") as capture:
                ui_e2e._record_tool_inventory("macos", root, root / "runner.log")
            self.assertEqual(
                [call.args[0] for call in capture.call_args_list],
                [["xcodebuild", "-version"], ["xcodebuild", "-showsdks"]],
            )

    def test_tvos_inventory_includes_runtime_and_devices(self) -> None:
        runtime = "com.apple.CoreSimulator.SimRuntime.tvOS-26-0"
        runtimes = {
            "runtimes": [{"identifier": runtime, "version": "26.0", "isAvailable": True}]
        }
        devices = {
            "devices": {
                runtime: [
                    {"isAvailable": True, "deviceTypeIdentifier": "Apple-TV-fixture"}
                ]
            }
        }

        def output(command: list[str], **_kwargs: object) -> str:
            if "runtimes" in command:
                return json.dumps(runtimes)
            if "devices" in command:
                return json.dumps(devices)
            return "tool metadata\n"

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(ui_e2e, "run_capture", side_effect=output) as capture:
                ui_e2e._record_tool_inventory("tvos", root, root / "runner.log")
            self.assertEqual(
                [call.args[0] for call in capture.call_args_list],
                [
                    ["xcodebuild", "-version"],
                    ["xcodebuild", "-showsdks"],
                    ["xcrun", "simctl", "list", "runtimes", "--json"],
                    ["xcrun", "simctl", "list", "devices", "available", "--json"],
                ],
            )

    def test_tvos_inventory_requires_available_runtime(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(ui_e2e, "run_capture", return_value="{}"):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "No installed, available tvOS"):
                    ui_e2e._record_tool_inventory("tvos", root, root / "runner.log")


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

    def test_xcodebuild_signing_is_platform_specific(self) -> None:
        for platform in ("macos", "tvos"):
            with self.subTest(platform=platform):
                command = ui_e2e.build_xcodebuild_command(
                    platform=platform,
                    server_address="http://127.0.0.1:43210",
                    app_bundle_identifier="dev.joey.tvos-net-player.uie2e." + "a" * 32,
                    derived_data_path=Path("/tmp/DerivedData"),
                    result_bundle_path=Path("/tmp/UIE2E.xcresult"),
                    destination="platform=macOS" if platform == "macos" else "platform=tvOS Simulator",
                )
                settings = dict(
                    argument.split("=", 1)
                    for argument in command
                    if argument.startswith(("CODE_SIGN", "DEVELOPMENT_TEAM="))
                )
                if platform == "macos":
                    self.assertEqual(
                        settings,
                        {
                            "CODE_SIGN_IDENTITY": "-",
                            "CODE_SIGNING_ALLOWED": "YES",
                            "CODE_SIGN_STYLE": "Manual",
                            "DEVELOPMENT_TEAM": "",
                        },
                    )
                else:
                    self.assertEqual(settings, {"CODE_SIGNING_ALLOWED": "NO"})

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
