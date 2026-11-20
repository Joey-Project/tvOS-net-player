from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("ui-e2e.py")
SPEC = importlib.util.spec_from_file_location("ui_e2e", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
ui_e2e = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ui_e2e
SPEC.loader.exec_module(ui_e2e)


def assert_darwin_pid_not_live(pid: int) -> None:
    """Independently observe one former descendant without requiring PID removal."""
    try:
        result = subprocess.run(
            ["ps", "-p", str(pid), "-o", "stat="],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=1,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise AssertionError("Darwin PID state query failed; cleanup is unverified.") from error
    if len(result.stdout) > 128 or len(result.stderr) > 128:
        raise AssertionError("Darwin PID state query exceeded its one-field output bound.")
    if result.returncode == 1 and not result.stdout and not result.stderr:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        except OSError as error:
            raise AssertionError("Empty ps output did not prove PID absence.") from error
        raise AssertionError("Empty ps output did not prove PID absence.")
    if result.returncode != 0 or result.stderr:
        raise AssertionError("Darwin PID state query failed; cleanup is unverified.")
    if re.fullmatch(rb"[ \t]*Z[+<>AELNSsVWX]*[ \t]*\n?", result.stdout) is None:
        raise AssertionError("Darwin PID state was not explicitly Z; cleanup is unverified.")


class ProcessSupervisorTests(unittest.TestCase):
    def test_exited_leader_descendant_is_terminated_and_leader_reaped(self) -> None:
        owned: list[object] = []
        popen = ui_e2e.subprocess.Popen

        def spawn(*args: object, **kwargs: object) -> object:
            process = popen(*args, **kwargs)
            owned.append(process)
            return process

        with tempfile.TemporaryDirectory() as directory:
            log_path = Path(directory) / "descendant.log"
            try:
                with mock.patch.object(ui_e2e.subprocess, "Popen", side_effect=spawn):
                    result = ui_e2e.run_process(
                        [sys.executable, "-c",
                         "import subprocess,sys; child=subprocess.Popen([sys.executable,'-c',"
                         "'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(60)']); print(child.pid,flush=True)"],
                        log_path=log_path,
                        timeout_seconds=0.3,
                        max_output_bytes=1024,
                    )
                self.assertTrue(result.timed_out)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(owned[0].wait(timeout=0), 0)
                descendant_pid = int(log_path.read_text().strip())
                if sys.platform.startswith("linux"):
                    # Container PID 1 may leave a terminated orphan as a zombie.
                    self.assertFalse(ui_e2e._group_has_live_members(owned[0].pid))
                elif sys.platform == "darwin":
                    assert_darwin_pid_not_live(descendant_pid)
                else:
                    with self.assertRaises(ProcessLookupError):
                        os.kill(descendant_pid, 0)
            finally:
                for process in owned:
                    if process.returncode is None:
                        ui_e2e.stop_process_group(process)

    def test_reaped_leader_cannot_authorize_signals_to_reused_group(self) -> None:
        process = mock.Mock(pid=4244, returncode=0)
        with (
            mock.patch.object(ui_e2e, "_group_has_live_members", return_value=True),
            mock.patch.object(ui_e2e.os, "killpg") as killpg,
        ):
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "unreserved PGID"):
                ui_e2e.stop_process_group(process)
        killpg.assert_not_called()
        process.wait.assert_not_called()

    def test_changed_group_identity_is_not_signalled(self) -> None:
        process = mock.Mock(pid=4245, returncode=None)
        with (
            mock.patch.object(ui_e2e, "_child_has_exited", return_value=False) as child_has_exited,
            mock.patch.object(ui_e2e.os, "getpgid", return_value=9999),
            mock.patch.object(ui_e2e.os, "killpg") as killpg,
        ):
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "original session/group leader"):
                ui_e2e.stop_process_group(process)
        self.assertEqual(
            child_has_exited.call_args_list,
            [mock.call(process), mock.call(process, require_fresh_waitid=True)],
        )
        killpg.assert_not_called()
        process.wait.assert_not_called()

    def test_exit_during_identity_check_keeps_unreaped_pid_anchor(self) -> None:
        process = mock.Mock(pid=4246, returncode=None)
        events: list[tuple[str, object]] = []
        exit_status = mock.Mock(si_pid=process.pid)

        def reap(*, timeout: float) -> int:
            events.append(("wait", process.returncode))
            process.returncode = 0
            return 0

        def signal_group(_pgid: int, signum: int) -> None:
            events.append(("signal", (process.returncode, signum)))

        expected_waitid = mock.call(
            ui_e2e.os.P_PID,
            process.pid,
            ui_e2e.os.WEXITED | ui_e2e.os.WNOHANG | ui_e2e.os.WNOWAIT,
        )
        process.wait.side_effect = reap
        with (
            mock.patch.object(ui_e2e.os, "waitid", side_effect=[None, exit_status, exit_status]) as waitid,
            mock.patch.object(ui_e2e.os, "getpgid", return_value=process.pid + 1),
            mock.patch.object(ui_e2e.os, "killpg", side_effect=signal_group),
            mock.patch.object(ui_e2e, "_group_has_live_members", return_value=False),
        ):
            ui_e2e.stop_process_group(process)

        self.assertEqual(waitid.call_args_list, [expected_waitid] * 3)
        self.assertEqual(
            events,
            [
                ("signal", (None, ui_e2e.signal.SIGTERM)),
                ("signal", (None, ui_e2e.signal.SIGKILL)),
                ("wait", None),
            ],
        )
        process.wait.assert_called_once_with(timeout=5)

    def test_revalidation_failure_does_not_signal_or_reap(self) -> None:
        for error, expected_error in (
            (ChildProcessError("already reaped"), ui_e2e.UIE2EError),
            (PermissionError("waitid denied"), PermissionError),
        ):
            with self.subTest(error=type(error).__name__):
                process = mock.Mock(pid=4247, returncode=None)
                with (
                    mock.patch.object(ui_e2e.os, "waitid", side_effect=[None, error]),
                    mock.patch.object(ui_e2e.os, "getpgid", return_value=9999),
                    mock.patch.object(ui_e2e.os, "killpg") as killpg,
                    self.assertRaises(expected_error) as caught,
                ):
                    ui_e2e.stop_process_group(process)
                if isinstance(caught.exception, ui_e2e.UIE2EError):
                    self.assertIn("reaped externally", str(caught.exception))
                killpg.assert_not_called()
                process.wait.assert_not_called()

    def test_reaped_during_waitid_revalidation_is_not_exit_proof(self) -> None:
        process = mock.Mock(pid=4248, returncode=None)
        waitid_calls = 0

        def waitid(*_args: object) -> object:
            nonlocal waitid_calls
            waitid_calls += 1
            if waitid_calls == 1:
                return None
            process.returncode = 0
            raise ChildProcessError("already reaped")

        with (
            mock.patch.object(ui_e2e.os, "waitid", side_effect=waitid),
            mock.patch.object(ui_e2e.os, "getpgid", return_value=9999),
            mock.patch.object(ui_e2e, "_group_has_live_members", return_value=True),
            mock.patch.object(ui_e2e.os, "killpg") as killpg,
            self.assertRaisesRegex(ui_e2e.UIE2EError, "reaped externally"),
        ):
            ui_e2e.stop_process_group(process)
        killpg.assert_not_called()
        process.wait.assert_not_called()

    def test_reaped_during_identity_check_with_live_group_is_not_signalled(self) -> None:
        process = mock.Mock(pid=4248, returncode=None)

        def reap_before_identity(_pid: int) -> int:
            process.returncode = 0
            return process.pid + 1

        with (
            mock.patch.object(ui_e2e, "_child_has_exited", return_value=False),
            mock.patch.object(ui_e2e.os, "getpgid", side_effect=reap_before_identity),
            mock.patch.object(ui_e2e, "_group_has_live_members", return_value=True) as has_live_members,
            mock.patch.object(ui_e2e.os, "killpg") as killpg,
            self.assertRaisesRegex(ui_e2e.UIE2EError, "leader already reaped"),
        ):
            ui_e2e.stop_process_group(process)
        has_live_members.assert_called_once_with(process.pid)
        killpg.assert_not_called()
        process.wait.assert_not_called()

    def test_reaped_during_identity_check_with_empty_group_returns_without_signalling(self) -> None:
        process = mock.Mock(pid=4249, returncode=None)

        def reap_before_identity(_pid: int) -> int:
            process.returncode = 0
            return process.pid + 1

        with (
            mock.patch.object(ui_e2e, "_child_has_exited", return_value=False),
            mock.patch.object(ui_e2e.os, "getpgid", side_effect=reap_before_identity),
            mock.patch.object(ui_e2e, "_group_has_live_members", return_value=False) as has_live_members,
            mock.patch.object(ui_e2e.os, "killpg") as killpg,
        ):
            ui_e2e.stop_process_group(process)
        has_live_members.assert_called_once_with(process.pid)
        killpg.assert_not_called()
        process.wait.assert_not_called()

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
        process.returncode = None
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
                mock.patch.object(ui_e2e.os, "getpgid", return_value=4242),
                mock.patch.object(ui_e2e.os, "getsid", return_value=4242),
                mock.patch.object(ui_e2e, "_child_has_exited", return_value=False),
                mock.patch.object(ui_e2e, "_group_has_live_members", return_value=True),
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
        process.returncode = None
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
                mock.patch.object(ui_e2e.os, "getpgid", return_value=4243),
                mock.patch.object(ui_e2e.os, "getsid", return_value=4243),
                mock.patch.object(ui_e2e, "_child_has_exited", return_value=False),
                mock.patch.object(ui_e2e, "_group_has_live_members", return_value=True),
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


class ProcessProgressTests(unittest.TestCase):
    def test_fixed_phase_records_use_fake_clock_and_preserve_terminal_space(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ui-progress.jsonl"
            for phase in ("fixture_build", "xcode_test"):
                with mock.patch.object(
                    ui_e2e.time, "monotonic", side_effect=(100.0, 100.0, 102.5, 105.0)
                ):
                    progress = ui_e2e._ProcessProgress(path, phase, 900)
                    progress.write(
                        "started", retained_bytes=0, last_output_age=None,
                        output_activity=None,
                    )
                    progress.write(
                        "periodic", retained_bytes=11, last_output_age=2.0,
                        output_activity=True,
                    )
                    progress.write(
                        "completed", retained_bytes=11, last_output_age=2.5,
                        output_activity=False, returncode=0, terminal=True,
                    )
                    progress.close()
            records = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual([record["phase"] for record in records], [
                "fixture_build", "fixture_build", "fixture_build",
                "xcode_test", "xcode_test", "xcode_test",
            ])
            self.assertEqual([record["event"] for record in records], [
                "started", "periodic", "completed",
                "started", "periodic", "completed",
            ])
            self.assertEqual(records[1]["elapsed_seconds"], 2.5)
            self.assertEqual(records[1]["last_output_age_seconds"], 2.0)
            self.assertTrue(records[1]["output_activity"])
            self.assertEqual(records[2]["retained_output_bytes"], 11)
            self.assertEqual(records[2]["child_returncode"], 0)
            self.assertLessEqual(path.stat().st_size, ui_e2e.MAX_PROGRESS_DIAGNOSTIC_BYTES)
            self.assertTrue(all("argv" not in record and "path" not in record for record in records))

    def test_progress_record_and_byte_caps_keep_both_terminal_records(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ui-progress.jsonl"
            with mock.patch.object(ui_e2e.time, "monotonic", return_value=1.0):
                for phase in ("fixture_build", "xcode_test"):
                    progress = ui_e2e._ProcessProgress(path, phase, 1800)
                    progress.write(
                        "started", retained_bytes=0, last_output_age=None,
                        output_activity=None,
                    )
                    for _ in range(100):
                        progress.write(
                            "periodic", retained_bytes=8192, last_output_age=None,
                            output_activity=False,
                        )
                    progress.write(
                        "completed", retained_bytes=8192, last_output_age=None,
                        output_activity=False, returncode=0, terminal=True,
                    )
                    progress.close()
            records = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertLessEqual(len(records), ui_e2e.MAX_PROGRESS_DIAGNOSTIC_RECORDS)
            self.assertLessEqual(path.stat().st_size, ui_e2e.MAX_PROGRESS_DIAGNOSTIC_BYTES)
            self.assertEqual(
                [record["event"] for record in records if record["event"] == "completed"],
                ["completed", "completed"],
            )

    def test_full_effective_budgets_sample_early_and_late_in_both_phases(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ui-progress.jsonl"
            clock = [0.0]
            with mock.patch.object(ui_e2e.time, "monotonic", side_effect=lambda: clock[0]):
                for phase, budget in (("fixture_build", 900), ("xcode_test", 1800)):
                    phase_start = clock[0]
                    progress = ui_e2e._ProcessProgress(path, phase, budget)
                    self.assertEqual(progress.periodic_interval_seconds, budget / 31)
                    progress.write(
                        "started", retained_bytes=0, last_output_age=None,
                        output_activity=None,
                    )
                    for sample in range(1, 31):
                        clock[0] = phase_start + sample * progress.periodic_interval_seconds
                        progress.write(
                            "periodic", retained_bytes=0, last_output_age=None,
                            output_activity=False,
                        )
                    clock[0] = phase_start + float(budget)
                    progress.write(
                        "deadline", retained_bytes=0, last_output_age=None,
                        output_activity=False, returncode=-15, terminal=True,
                    )
                    progress.close()
            records = [json.loads(line) for line in path.read_text().splitlines()]
            for phase, budget in (("fixture_build", 900), ("xcode_test", 1800)):
                phase_records = [record for record in records if record["phase"] == phase]
                periodic = [record for record in phase_records if record["event"] == "periodic"]
                self.assertEqual(len(periodic), 30)
                self.assertAlmostEqual(periodic[0]["elapsed_seconds"], budget / 31, places=3)
                self.assertAlmostEqual(periodic[-1]["elapsed_seconds"], budget * 30 / 31, places=3)
                self.assertEqual(phase_records[0]["event"], "started")
                self.assertEqual(phase_records[-1]["event"], "deadline")
                self.assertEqual(phase_records[-1]["effective_budget_seconds"], budget)
                self.assertEqual(phase_records[-1]["child_returncode"], -15)
            self.assertEqual(len(records), 64)
            self.assertLessEqual(path.stat().st_size, ui_e2e.MAX_PROGRESS_DIAGNOSTIC_BYTES)

    def test_unexpected_event_labels_are_omitted_without_raising(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ui-progress.jsonl"
            with mock.patch.object(ui_e2e.time, "monotonic", return_value=1.0):
                progress = ui_e2e._ProcessProgress(path, "fixture_build", 900)
                progress.write(
                    "private raw error", retained_bytes=1, last_output_age=None,
                    output_activity=False,
                )
                progress.write(
                    "completed", retained_bytes=1, last_output_age=None,
                    output_activity=False, returncode=0, terminal=True,
                )
                progress.close()
            records = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual([record["event"] for record in records], ["completed"])
            self.assertNotIn("private", path.read_text())

    def test_hardlinked_progress_file_is_rejected_before_chmod(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = root / "original"
            progress_path = root / "ui-progress.jsonl"
            original.write_text("existing", encoding="utf-8")
            original.chmod(0o644)
            os.link(original, progress_path)
            original_mode = original.stat().st_mode
            with mock.patch.object(ui_e2e.time, "monotonic", return_value=1.0):
                progress = ui_e2e._ProcessProgress(progress_path, "xcode_test", 1800)
                progress.write(
                    "completed", retained_bytes=0, last_output_age=None,
                    output_activity=None, returncode=0, terminal=True,
                )
                progress.close()
            self.assertTrue(progress.disabled)
            self.assertEqual(original.stat().st_mode, original_mode)
            self.assertEqual(original.read_text(encoding="utf-8"), "existing")

    def test_descriptor_is_closed_when_permission_setup_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ui-progress.jsonl"
            with (
                mock.patch.object(ui_e2e.time, "monotonic", return_value=1.0),
                mock.patch.object(ui_e2e.os, "fchmod", side_effect=PermissionError("private")),
                mock.patch.object(ui_e2e.os, "close", wraps=os.close) as close,
            ):
                progress = ui_e2e._ProcessProgress(path, "fixture_build", 900)
            self.assertTrue(progress.disabled)
            self.assertIsNone(progress.file)
            close.assert_called_once()

    def test_process_outcomes_and_unknown_states_are_recorded_without_raw_errors(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cases = (
                ("success", "print('ok')", 0, "completed", False),
                ("nonzero", "raise SystemExit(7)", 7, "completed", False),
                ("deadline", "import time; time.sleep(5)", None, "deadline", True),
                ("output", "print('x' * 4096)", None, "output_limited", True),
            )
            for name, code, expected_status, expected_event, limited in cases:
                with self.subTest(name=name):
                    progress_path = root / f"{name}.jsonl"
                    result = ui_e2e.run_process(
                        [sys.executable, "-c", code],
                        log_path=root / f"{name}.log",
                        timeout_seconds=0.1 if name == "deadline" else 5,
                        max_output_bytes=16 if name == "output" else 1024,
                        progress_path=progress_path,
                        progress_phase="xcode_test",
                    )
                    self.assertEqual(result.timed_out, name == "deadline")
                    self.assertEqual(result.output_limited, limited and name == "output")
                    terminal = json.loads(progress_path.read_text().splitlines()[-1])
                    self.assertEqual(terminal["event"], expected_event)
                    self.assertEqual(
                        terminal["child_returncode"],
                        result.returncode if expected_status is None else expected_status,
                    )
                    self.assertEqual(terminal["retained_output_bytes"], result.bytes_written)
                    self.assertEqual(terminal["last_output_age_seconds"] is None, result.bytes_written == 0)
                    self.assertNotIn("ok", progress_path.read_text())
                    self.assertLessEqual(progress_path.stat().st_size, ui_e2e.MAX_PROGRESS_DIAGNOSTIC_BYTES)

    def test_spawn_and_cleanup_errors_remain_errors_and_keep_unknown_status(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spawn_path = root / "spawn.jsonl"
            with (
                mock.patch.object(ui_e2e.subprocess, "Popen", side_effect=OSError("private spawn detail")),
                self.assertRaises(OSError),
            ):
                ui_e2e.run_process(
                    ["private-command"], log_path=root / "spawn.log",
                    timeout_seconds=2, max_output_bytes=128,
                    progress_path=spawn_path, progress_phase="fixture_build",
                )
            spawned = [json.loads(line) for line in spawn_path.read_text().splitlines()]
            self.assertEqual(spawned[-1]["event"], "error")
            self.assertIsNone(spawned[-1]["child_returncode"])
            self.assertNotIn("private", spawn_path.read_text())

            cleanup_path = root / "cleanup.jsonl"
            owned: list[subprocess.Popen[bytes]] = []
            popen = ui_e2e.subprocess.Popen

            def spawn(*args: object, **kwargs: object) -> subprocess.Popen[bytes]:
                process = popen(*args, **kwargs)
                owned.append(process)
                return process

            try:
                with (
                    mock.patch.object(ui_e2e.subprocess, "Popen", side_effect=spawn),
                    mock.patch.object(ui_e2e, "stop_process_group", side_effect=PermissionError("private cleanup detail")),
                    self.assertRaisesRegex(ui_e2e.UIE2EError, "cleanup unverified"),
                ):
                    ui_e2e.run_process(
                        [sys.executable, "-c", "print('x')"], log_path=root / "cleanup.log",
                        timeout_seconds=2, max_output_bytes=128,
                        progress_path=cleanup_path, progress_phase="fixture_build",
                    )
            finally:
                for process in owned:
                    if process.returncode is None:
                        process.wait(timeout=2)
            terminal = json.loads(cleanup_path.read_text().splitlines()[-1])
            self.assertEqual(terminal["event"], "error")
            self.assertTrue(terminal["cleanup_failed"])
            self.assertNotIn("private", cleanup_path.read_text())

    def test_default_process_callers_do_not_create_progress_records(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = ui_e2e.run_process(
                [sys.executable, "-c", "print('ordinary')"],
                log_path=root / "ordinary.log", timeout_seconds=2,
                max_output_bytes=128,
            )
            self.assertEqual(result.returncode, 0)
            self.assertFalse((root / "ui-progress.jsonl").exists())


class DarwinPIDStateAssertionTests(unittest.TestCase):
    def test_explicit_zombie_state_is_terminal(self) -> None:
        for output in (
            b"Z", b"Z\n", b"   Z \n", b"\tZ\n",
            b"Z+\n", b"Zs\n", b"Z+N\n", b"Z+<>AELNSsVWX\n",
        ):
            with (
                self.subTest(output=output),
                mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 0, output, b""
                )) as query,
                mock.patch.object(os, "kill") as kill,
            ):
                assert_darwin_pid_not_live(4244)
                query.assert_called_once_with(
                    ["ps", "-p", "4244", "-o", "stat="],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=1,
                    check=False,
                )
                kill.assert_not_called()

    def test_live_or_unknown_states_are_not_terminal(self) -> None:
        for state in (
            b"R", b"S", b"I", b"T", b"U", b"D", b"X", b"x", b"?",
            b"R+", b"Ss", b"Z?", b"ZZ", b"Zinvalid",
        ):
            with (
                self.subTest(state=state),
                mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 0, state + b"\n", b""
                )) as query,
                mock.patch.object(os, "kill") as kill,
                self.assertRaisesRegex(AssertionError, "not explicitly Z"),
            ):
                assert_darwin_pid_not_live(4244)
            query.assert_called_once()
            kill.assert_not_called()

    def test_malformed_or_oversized_state_is_not_terminal(self) -> None:
        for output in (b"", b"\n", b"z\n", b"Z\nR\n", b"Z invalid\n", b"Z\x00", b" " * 129):
            with (
                self.subTest(output=output[:32]),
                mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 0, output, b""
                )) as query,
                mock.patch.object(os, "kill") as kill,
                self.assertRaises(AssertionError),
            ):
                assert_darwin_pid_not_live(4244)
            query.assert_called_once()
            kill.assert_not_called()

    def test_failed_or_denied_query_is_not_absence(self) -> None:
        for returncode, stdout, stderr in (
            (1, b"", b"operation denied"),
            (1, b"\n", b""),
            (2, b"", b""),
            (-9, b"", b""),
            (0, b"Z\n", b"warning"),
            (0, b"Z\n", b"x" * 129),
        ):
            with (
                self.subTest(returncode=returncode, stderr_bytes=len(stderr)),
                mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], returncode, stdout, stderr
                )) as query,
                mock.patch.object(os, "kill") as kill,
                self.assertRaises(AssertionError),
            ):
                assert_darwin_pid_not_live(4244)
            query.assert_called_once()
            kill.assert_not_called()

    def test_query_errors_fail_closed_without_retry(self) -> None:
        for error in (
            PermissionError("query denied"),
            FileNotFoundError("ps unavailable"),
            OSError("query failed"),
            subprocess.TimeoutExpired(["ps"], 1),
        ):
            with (
                self.subTest(error=type(error).__name__),
                mock.patch.object(subprocess, "run", side_effect=error) as query,
                mock.patch.object(os, "kill") as kill,
                self.assertRaisesRegex(AssertionError, "query failed"),
            ):
                assert_darwin_pid_not_live(4244)
            query.assert_called_once()
            kill.assert_not_called()

    def test_empty_ps_and_esrch_together_prove_absence(self) -> None:
        with (
            mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                [], 1, b"", b""
            )) as query,
            mock.patch.object(os, "kill", side_effect=ProcessLookupError()) as kill,
        ):
            assert_darwin_pid_not_live(4244)
        query.assert_called_once()
        kill.assert_called_once_with(4244, 0)

    def test_empty_ps_with_existing_or_denied_pid_is_unverified(self) -> None:
        for error in (None, PermissionError("signal zero denied"), OSError("query failed")):
            with (
                self.subTest(error=type(error).__name__),
                mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 1, b"", b""
                )) as query,
                mock.patch.object(os, "kill", side_effect=error) as kill,
                self.assertRaisesRegex(AssertionError, "did not prove PID absence"),
            ):
                assert_darwin_pid_not_live(4244)
            query.assert_called_once()
            kill.assert_called_once_with(4244, 0)

    @contextlib.contextmanager
    def owned_child(self):
        child = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdin.buffer.read(1)"],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            yield child
        finally:
            if child.stdin is not None and not child.stdin.closed:
                child.stdin.close()
            try:
                child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                child.kill()  # This unreaped direct child still reserves its PID.
                child.wait(timeout=2)

    @unittest.skipUnless(sys.platform == "darwin", "Darwin ps state assertion")
    def test_owned_live_child_is_rejected_even_when_signal_zero_succeeds(self) -> None:
        with self.owned_child() as child:
            os.kill(child.pid, 0)
            with self.assertRaisesRegex(AssertionError, "not explicitly Z"):
                assert_darwin_pid_not_live(child.pid)
            self.assertIsNone(child.returncode)

    @unittest.skipUnless(
        sys.platform == "darwin" and hasattr(os, "waitid") and hasattr(os, "WNOWAIT"),
        "Darwin waitid(WNOWAIT) is required to reserve a zombie PID",
    )
    def test_owned_unreaped_zombie_is_accepted_even_when_signal_zero_succeeds(self) -> None:
        with self.owned_child() as child:
            self.assertIsNotNone(child.stdin)
            child.stdin.close()
            # Prepare a controlled exit; the state assertion itself never retries.
            deadline = time.monotonic() + 2
            status = None
            while time.monotonic() < deadline:
                status = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                if status is not None:
                    break
                time.sleep(0.01)
            self.assertIsNotNone(status, "The owned child did not exit within two seconds.")
            self.assertEqual(status.si_pid, child.pid)
            self.assertIsNone(child.returncode)
            os.kill(child.pid, 0)
            assert_darwin_pid_not_live(child.pid)


class RunCaptureTests(unittest.TestCase):
    def test_capture_forwards_cwd_to_real_subprocess(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            working_dir = root / "working"
            working_dir.mkdir()
            output = ui_e2e.run_capture(
                [sys.executable, "-c", "import os; print(os.getcwd())"],
                logs_dir=root / "logs",
                name="capture-cwd",
                timeout_seconds=5,
                max_output_bytes=4096,
                cwd=working_dir,
            )
            self.assertEqual(Path(output.strip()), working_dir.resolve())
            self.assertEqual((root / "logs" / "capture-cwd.log").read_text(), output)

    def test_capture_keeps_real_deadline_and_output_bounds_with_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, code, timeout, limit, message in (
                ("deadline", "import time; time.sleep(10)", 0.1, 1024, "deadline"),
                ("output", "print('x' * 8192)", 5, 128, "output limit"),
            ):
                with self.subTest(name=name), self.assertRaisesRegex(
                    ui_e2e.UIE2EError, message
                ):
                    ui_e2e.run_capture(
                        [sys.executable, "-c", code],
                        logs_dir=root / "logs",
                        name=name,
                        timeout_seconds=timeout,
                        max_output_bytes=limit,
                        cwd=root,
                    )
                self.assertLessEqual((root / "logs" / f"{name}.log").stat().st_size, limit)


class LinuxGroupStateTests(unittest.TestCase):
    @contextlib.contextmanager
    def proc_view(self, rows: list[tuple[int, str, int]]):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for pid, state, pgid in rows:
                process_dir = root / str(pid)
                process_dir.mkdir()
                (process_dir / "stat").write_text(
                    f"{pid} (worker ) (nested) name) {state} 1 {pgid} {pgid} 0 0\n",
                    encoding="ascii",
                )
            with (
                mock.patch.object(ui_e2e.sys, "platform", "linux"),
                mock.patch.object(ui_e2e, "_LINUX_PROC_ROOT", root),
                mock.patch.object(ui_e2e.os, "killpg") as killpg,
            ):
                yield root, killpg

    def test_zombie_only_and_dead_group_is_terminated(self) -> None:
        rows = [(101, "Z", 4244), (102, "X", 4244), (103, "S", 9999)]
        with self.proc_view(rows) as (_, killpg):
            self.assertFalse(ui_e2e._group_has_live_members(4244))
            killpg.assert_called_once_with(4244, 0)

    def test_live_group_member_is_not_mistaken_for_zombie(self) -> None:
        for state in ("R", "S", "D", "T", "t", "I"):
            with (
                self.subTest(state=state),
                self.proc_view([(101, "Z", 4244), (102, state, 4244)]),
            ):
                self.assertTrue(ui_e2e._group_has_live_members(4244))

    def test_unreadable_or_malformed_stat_is_not_absence(self) -> None:
        with self.proc_view([(101, "Z", 4244)]) as (root, _):
            for error in (PermissionError("denied"), OSError("read failed")):
                with (
                    self.subTest(error=type(error).__name__),
                    mock.patch.object(Path, "open", side_effect=error),
                ):
                    with self.assertRaises(type(error)) as caught:
                        ui_e2e._group_has_live_members(4244)
                    self.assertIs(caught.exception, error)
            for content in (b"101 (bad) Z 1 invalid", b"bad fields", b"x" * 4097):
                with self.subTest(content=content[:32]):
                    (root / "101" / "stat").write_bytes(content)
                    with self.assertRaisesRegex(ui_e2e.UIE2EError, "Cleanup unverified"):
                        ui_e2e._group_has_live_members(4244)

    def test_disappearing_process_is_tolerated_but_hidden_group_is_unverified(self) -> None:
        with self.proc_view([(101, "Z", 4244), (102, "S", 4244)]) as (root, _):
            (root / "102" / "stat").unlink()
            self.assertFalse(ui_e2e._group_has_live_members(4244))
            (root / "101" / "stat").unlink()
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "no visible members"):
                ui_e2e._group_has_live_members(4244)

    def test_missing_group_needs_no_proc_query_and_query_errors_propagate(self) -> None:
        with self.proc_view([]) as (_, killpg):
            killpg.side_effect = ProcessLookupError()
            with mock.patch.object(ui_e2e.os, "scandir") as scan:
                self.assertFalse(ui_e2e._group_has_live_members(4244))
                scan.assert_not_called()
            killpg.side_effect = None
            errors = [PermissionError("proc denied"), FileNotFoundError("no proc mount")]
            for error in errors:
                with (
                    self.subTest(error=type(error).__name__),
                    mock.patch.object(ui_e2e.os, "scandir", side_effect=error),
                ):
                    with self.assertRaises(type(error)) as caught:
                        ui_e2e._group_has_live_members(4244)
                    self.assertIs(caught.exception, error)

    def test_linux_proc_query_has_entry_and_time_bounds(self) -> None:
        with self.proc_view([(101, "Z", 4244)]):
            with mock.patch.object(ui_e2e.time, "monotonic", side_effect=[0.0, 1.0]):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "4096-entry/1-second cap"):
                    ui_e2e._group_has_live_members(4244)
            entry = mock.Mock()
            entry.name = "self"
            scan = mock.MagicMock()
            scan.__enter__.return_value = iter([entry] * 4097)
            with (
                mock.patch.object(ui_e2e.time, "monotonic", return_value=0.0),
                mock.patch.object(ui_e2e.os, "scandir", return_value=scan),
                self.assertRaisesRegex(ui_e2e.UIE2EError, "4096-entry/1-second cap"),
            ):
                ui_e2e._group_has_live_members(4244)

    def test_leader_is_reaped_and_zombie_descendant_requires_no_more_signals(self) -> None:
        process = mock.Mock(pid=4244, returncode=None)
        process.wait.side_effect = lambda **kwargs: setattr(process, "returncode", 0)
        with (
            self.proc_view([(101, "Z", 4244)]) as (_, killpg),
            mock.patch.object(ui_e2e, "_child_has_exited", return_value=True),
        ):
            ui_e2e.stop_process_group(process)
            process.wait.assert_called_once_with(timeout=5)
            self.assertEqual(killpg.call_args_list, [
                mock.call(4244, ui_e2e.signal.SIGTERM),
                mock.call(4244, ui_e2e.signal.SIGKILL),
                mock.call(4244, 0),
            ])

    def test_reaped_leader_with_live_group_still_cannot_authorize_signals(self) -> None:
        process = mock.Mock(pid=4244, returncode=0)
        with self.proc_view([(101, "S", 4244)]) as (_, killpg):
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "unreserved PGID"):
                ui_e2e.stop_process_group(process)
            killpg.assert_called_once_with(4244, 0)
            process.wait.assert_not_called()


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


class WorkBudgetTests(unittest.TestCase):
    def test_absent_job_deadline_uses_local_cap(self) -> None:
        with mock.patch.object(ui_e2e.time, "monotonic", return_value=100):
            self.assertEqual(ui_e2e.resolve_work_deadline(), 100 + 35 * 60)
        self.assertEqual(ui_e2e.FIXTURE_BUILD_TIMEOUT_SECONDS, 15 * 60)

    def test_earlier_job_deadline_limits_local_work(self) -> None:
        with mock.patch.object(ui_e2e.time, "monotonic", return_value=100):
            self.assertEqual(ui_e2e.resolve_work_deadline("200"), 200)

    def test_later_job_deadline_cannot_extend_local_cap(self) -> None:
        with mock.patch.object(ui_e2e.time, "monotonic", return_value=100):
            self.assertEqual(ui_e2e.resolve_work_deadline("10000"), 100 + 35 * 60)

    def test_expired_job_deadlines_are_rejected(self) -> None:
        for deadline in ("100", "99", "0", "-1"):
            with (
                self.subTest(deadline=deadline),
                mock.patch.object(ui_e2e.time, "monotonic", return_value=100),
                self.assertRaisesRegex(ui_e2e.UIE2EError, "work budget has expired"),
            ):
                ui_e2e.resolve_work_deadline(deadline)

    def test_malformed_and_nonfinite_job_deadlines_are_rejected_without_values(self) -> None:
        for deadline in ("", "untrusted-deadline-value", "NaN", "Inf", "-Inf", "1e999"):
            with self.subTest(deadline=deadline), mock.patch.object(
                ui_e2e.time, "monotonic", return_value=100
            ):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "work budget is invalid") as caught:
                    ui_e2e.resolve_work_deadline(deadline)
                if deadline:
                    self.assertNotIn(deadline, str(caught.exception))

    def test_remaining_work_is_checked_at_phase_boundaries(self) -> None:
        with mock.patch.object(ui_e2e.time, "monotonic", return_value=100):
            self.assertEqual(ui_e2e.require_work_remaining(105), 5)
            for deadline in (100, 99):
                with self.subTest(deadline=deadline), self.assertRaises(ui_e2e.UIE2EError):
                    ui_e2e.require_work_remaining(deadline)

    def test_invalid_or_expired_budget_stages_failure_before_workspace_removal(self) -> None:
        for deadline in ("untrusted-deadline-value", "99"):
            with self.subTest(deadline=deadline), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                work = root / "work"
                work.mkdir()
                artifacts = root / "artifacts"
                with (
                    mock.patch.object(ui_e2e.sys, "platform", "darwin"),
                    mock.patch.object(ui_e2e.time, "monotonic", return_value=100),
                    mock.patch.dict(
                        ui_e2e.os.environ,
                        {"UI_E2E_JOB_WORK_DEADLINE_MONOTONIC": deadline},
                    ),
                    mock.patch.object(ui_e2e, "_artifact_dir", return_value=artifacts),
                    mock.patch.object(ui_e2e, "_private_work_dir", return_value=work),
                    mock.patch.object(ui_e2e, "mute_system_audio", autospec=True) as mute,
                    self.assertRaisesRegex(ui_e2e.UIE2EError, "work budget"),
                ):
                    ui_e2e.run_ui_e2e("macos")
                mute.assert_not_called()
                self.assertFalse(work.exists())
                failure = (artifacts / "runner.log").read_text()
                self.assertIn("failed: UIE2EError: UI E2E work budget", failure)
                self.assertNotIn(deadline, failure)

    def test_budget_expiring_during_fixture_build_prevents_media_work(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work = root / "work"
            work.mkdir()
            artifacts = root / "artifacts"
            with (
                mock.patch.object(ui_e2e.sys, "platform", "darwin"),
                mock.patch.object(ui_e2e.time, "monotonic", return_value=100) as clock,
                mock.patch.dict(
                    ui_e2e.os.environ,
                    {"UI_E2E_JOB_WORK_DEADLINE_MONOTONIC": "110"},
                ),
                mock.patch.object(ui_e2e, "_artifact_dir", return_value=artifacts),
                mock.patch.object(ui_e2e, "_private_work_dir", return_value=work),
                mock.patch.object(ui_e2e, "mute_system_audio", autospec=True),
                mock.patch.object(ui_e2e, "_record_tool_inventory", autospec=True),
                mock.patch.object(ui_e2e, "verify_scheme_contract", autospec=True),
                mock.patch.object(ui_e2e, "create_fixture_media", autospec=True) as media,
            ):
                def finish_build(*args: object, **kwargs: object) -> Path:
                    clock.return_value = 111
                    return root / "fixture"

                with (
                    mock.patch.object(
                        ui_e2e, "build_task_fixture", autospec=True, side_effect=finish_build
                    ) as build,
                    self.assertRaisesRegex(ui_e2e.UIE2EError, "work budget has expired"),
                ):
                    ui_e2e.run_ui_e2e("macos")
                build.assert_called_once_with(work / "logs", timeout_seconds=10)
                media.assert_not_called()
            self.assertFalse(work.exists())
            self.assertIn("work budget has expired", (artifacts / "runner.log").read_text())

    def test_workflow_deadline_is_first_and_exports_same_host_monotonic_time(self) -> None:
        workflow = (ui_e2e.ROOT / ".github" / "workflows" / "ui-e2e.yml").read_text()
        self.assertIn("    timeout-minutes: 60\n", workflow.split("    steps:\n", 1)[0])
        first_step = workflow.split("    steps:\n", 1)[1].split("\n      - name:", 1)[0]
        lines = first_step.splitlines()
        self.assertEqual(lines[0], "      - name: Establish UI E2E work deadline")
        self.assertEqual(lines[1], "        run: |")
        fragment = "\n".join(line[10:] for line in lines[2:] if line.startswith("          "))
        self.assertEqual(len(fragment.splitlines()), 2)
        ui_e2e.subprocess.run(
            ["bash", "-n"], input=fragment, text=True, capture_output=True, check=True, timeout=5
        )
        with tempfile.TemporaryDirectory() as directory:
            env_file = Path(directory) / "github-env"
            env = ui_e2e.os.environ.copy()
            env["GITHUB_ENV"] = str(env_file)
            started = ui_e2e.time.monotonic()
            ui_e2e.subprocess.run(
                ["bash", "-e", "-o", "pipefail", "-c", fragment],
                env=env, text=True, capture_output=True, check=True, timeout=5,
            )
            name, value = env_file.read_text().strip().split("=", 1)
            self.assertEqual(name, "UI_E2E_JOB_WORK_DEADLINE_MONOTONIC")
            self.assertGreaterEqual(float(value), started + 45 * 60)
            self.assertLessEqual(float(value), ui_e2e.time.monotonic() + 45 * 60)


class ShellModeTests(unittest.TestCase):
    def run_wrapper(self, arguments: list[str], extra_env: dict[str, str] | None = None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / "scripts"
            bin_dir = root / "bin"
            scripts.mkdir()
            bin_dir.mkdir()
            shutil.copyfile(Path(__file__).with_name("test-ui-e2e.sh"), scripts / "test-ui-e2e.sh")
            build_capture = root / "build-env"
            call_capture = root / "python-args"
            (scripts / "build-cache-server.sh").write_text(
                '#!/bin/sh\nprintf "%s|%s\\n" "${LOCAL_BUILD_MODE-unset}" "${CI-unset}" > "$BUILD_CAPTURE"\n',
                encoding="utf-8",
            )
            (bin_dir / "python3").write_text(
                '#!/bin/sh\nprintf "%s\\n" "$@" > "$UI_E2E_CAPTURE"\n',
                encoding="utf-8",
            )
            (scripts / "build-cache-server.sh").chmod(0o755)
            (bin_dir / "python3").chmod(0o755)
            env = os.environ.copy()
            for key in ("CI", "LOCAL_BUILD_MODE", "CACHE_SERVER_BINARY"):
                env.pop(key, None)
            env.update(extra_env or {})
            env.update({
                "PATH": f"{bin_dir}:{env['PATH']}",
                "BUILD_CAPTURE": str(build_capture),
                "UI_E2E_CAPTURE": str(call_capture),
            })
            result = subprocess.run(
                ["bash", str(scripts / "test-ui-e2e.sh"), *arguments],
                env=env,
                text=True,
                capture_output=True,
                timeout=10,
                check=False,
            )
            calls = call_capture.read_text().splitlines() if call_capture.exists() else []
            builder = build_capture.read_text().strip() if build_capture.exists() else None
            return result, calls, builder

    def test_default_macos_selects_local_signed_outside_isolated_environment(self) -> None:
        result, calls, builder = self.run_wrapper(["macos"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[-4:], ["--platform", "macos", "--mode", "local-signed"])
        self.assertIsNone(builder)

    def test_explicit_local_signed_overrides_isolated_default_environment(self) -> None:
        result, calls, builder = self.run_wrapper(
            ["macos", "local-signed"], {"LOCAL_BUILD_MODE": "isolated"}
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[-4:], ["--platform", "macos", "--mode", "local-signed"])
        self.assertIsNone(builder)

    def test_isolated_modes_propagate_mode_into_cache_server_builder(self) -> None:
        for arguments in (["macos", "isolated"], ["tvos"]):
            with self.subTest(arguments=arguments):
                result, calls, builder = self.run_wrapper(list(arguments))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(builder, "isolated|unset")
                self.assertEqual(calls[-4:], ["--platform", arguments[0], "--mode", "isolated"])

    def test_ci_and_environment_defaults_remain_isolated(self) -> None:
        for env in (
            {"CI": "true"},
            {"LOCAL_BUILD_MODE": "isolated"},
            {"CI": "true", "LOCAL_BUILD_MODE": "isolated"},
        ):
            with self.subTest(env=env):
                result, calls, builder = self.run_wrapper(["macos"], dict(env))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls[-4:], ["--platform", "macos", "--mode", "isolated"])
                self.assertTrue(builder.startswith("isolated|"))

    def test_local_signed_ci_or_tvos_rejection_precedes_build_and_python(self) -> None:
        for arguments, env in ((["macos", "local-signed"], {"CI": "true"}),
                               (["macos", "local-signed"], {"CI": "true", "LOCAL_BUILD_MODE": "isolated"}),
                               (["tvos", "local-signed"], {})):
            with self.subTest(arguments=arguments, env=env):
                result, calls, builder = self.run_wrapper(arguments, env)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(calls, [])
                self.assertIsNone(builder)


class ImportClosureTests(unittest.TestCase):
    def test_cold_import_and_validation_work_without_local_build_companion(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            temporary = Path(directory)
            isolated = temporary / "neutral"
            isolated.mkdir()
            helper = isolated / "ui-e2e.py"
            shutil.copyfile(SCRIPT, helper)
            probe = (
                "import importlib.util, pathlib, sys; "
                "path = pathlib.Path(sys.argv[1]); "
                "spec = importlib.util.spec_from_file_location('isolated_ui_e2e', path); "
                "module = importlib.util.module_from_spec(spec); "
                "sys.modules[spec.name] = module; spec.loader.exec_module(module); "
                "assert str(path.parent) not in sys.path; "
                "assert 'local_build' not in sys.modules; "
                "assert module.validate_server_address('http://127.0.0.1:8080') == 'http://127.0.0.1:8080'"
            )
            result = subprocess.run(
                [sys.executable, "-I", "-c", probe, str(helper)],
                cwd=isolated,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))


class LocalSignedContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.signing_config = {
            "durable_root": self.root / "LocalBuild",
            "team_id": "TESTTEAM01",
            "identity_sha1": "A" * 40,
            "certificate_sha1": "A" * 40,
        }
        self.session_root = ui_e2e.local_signed_session_root(self.signing_config)
        self.session_root.mkdir(parents=True)
        self.derived = self.session_root / "DerivedData"
        self.paths = ui_e2e.local_signed_product_paths(self.derived)

    def product_receipt(self) -> dict[str, dict]:
        return {
            name: {
                "path": str(path),
                "identifier": ui_e2e.local_signed_bundle_identifiers()[name],
                "team_id": self.signing_config["team_id"],
                "certificate_sha1": self.signing_config["certificate_sha1"],
                "designated_requirement": f'identifier "{name}" and anchor apple',
                "cdhash": f"{name}-cdhash",
            }
            for name, path in self.paths.items()
        }

    def make_product_bundles(self, paths: dict[str, Path]) -> None:
        for name, path in paths.items():
            contents = path / "Contents"
            contents.mkdir(parents=True, exist_ok=True)
            with (contents / "Info.plist").open("wb") as stream:
                ui_e2e.local_build.plistlib.dump({
                    "CFBundleIdentifier": ui_e2e.local_signed_bundle_identifiers()[name],
                }, stream)

    def test_python_cli_defaults_to_isolated_mode(self) -> None:
        with mock.patch.object(ui_e2e, "run_ui_e2e", return_value=0) as run:
            self.assertEqual(ui_e2e.main(["--platform", "macos"]), 0)
        run.assert_called_once_with("macos", mode="isolated")

    def test_local_signed_rejections_happen_before_profile_or_build_access(self) -> None:
        cases = (("macos", "true", "darwin"), ("tvos", "", "darwin"), ("macos", "", "linux"))
        for platform, ci, system in cases:
            with self.subTest(platform=platform, ci=ci, system=system), \
                    mock.patch.object(ui_e2e.sys, "platform", system), \
                    mock.patch.dict(os.environ, {"CI": ci}, clear=True), \
                    mock.patch.object(ui_e2e.local_build, "load_config") as load_config, \
                    mock.patch.object(ui_e2e, "run_process") as run_process:
                with self.assertRaises(ui_e2e.UIE2EError):
                    ui_e2e.run_ui_e2e(platform, mode="local-signed")
                load_config.assert_not_called()
                run_process.assert_not_called()

    def test_bad_local_profile_and_unverified_server_never_fall_back(self) -> None:
        with mock.patch.object(ui_e2e.sys, "platform", "darwin"), \
                mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch.object(ui_e2e.local_build, "load_config", side_effect=ui_e2e.local_build.BuildError("invalid profile")), \
                mock.patch.object(ui_e2e, "run_process") as run_process:
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "Pinned local signing profile"):
                ui_e2e.run_ui_e2e("macos", mode="local-signed")
            run_process.assert_not_called()

        expected = self.signing_config["durable_root"] / ui_e2e.local_build.PRODUCTS["server"]
        with mock.patch.dict(os.environ, {"CACHE_SERVER_BINARY": str(self.root / "arbitrary-server")}, clear=True), \
                mock.patch.object(ui_e2e.local_build, "verify_existing_product") as verify, \
                mock.patch.object(ui_e2e, "run_process") as run_process:
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "pinned stable local-build server path"):
                ui_e2e.prepare_local_signed_server(
                    self.signing_config,
                    deadline=ui_e2e.time.monotonic() + 60,
                )
            verify.assert_not_called()
            run_process.assert_not_called()
        self.assertTrue(str(expected).startswith(str(self.root)))

    def test_local_server_reuses_only_receipted_fixed_product_without_building(self) -> None:
        expected = self.signing_config["durable_root"] / ui_e2e.local_build.PRODUCTS["server"]
        expected.parent.mkdir(parents=True)
        expected.write_bytes(b"signed server fixture")
        expected.chmod(0o755)
        deadline = ui_e2e.time.monotonic() + 60
        for override in (None, str(expected)):
            env = {} if override is None else {"CACHE_SERVER_BINARY": override}
            with self.subTest(override=override), \
                    mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch.object(ui_e2e.local_build, "verify_existing_product", return_value={"path": str(expected)}) as verify, \
                    mock.patch.object(ui_e2e, "run_process") as run_process:
                self.assertEqual(
                    ui_e2e.prepare_local_signed_server(self.signing_config, deadline=deadline),
                    expected,
                )
                verify.assert_called_once_with(
                    "server", deadline=deadline, allow_historical_source=True
                )
                run_process.assert_not_called()

    def test_missing_or_invalid_server_receipt_fails_without_build_fallback(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch.object(
                    ui_e2e.local_build,
                    "verify_existing_product",
                    side_effect=ui_e2e.local_build.BuildError("local build receipt is missing"),
                ), \
                mock.patch.object(ui_e2e, "run_process") as run_process:
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "Pinned local cache-server verification failed"):
                ui_e2e.prepare_local_signed_server(
                    self.signing_config,
                    deadline=ui_e2e.time.monotonic() + 60,
                )
            run_process.assert_not_called()

    def test_local_signed_bundle_ids_and_product_paths_are_fixed(self) -> None:
        self.assertEqual(ui_e2e.local_signed_bundle_identifiers(), {
            "app": "dev.joey.tvos-net-player.uie2e.local.macos",
            "uitests": "dev.joey.tvos-net-player.uie2e.local.macos.uitests",
            "runner": "dev.joey.tvos-net-player.uie2e.local.macos.uitests.xctrunner",
        })
        self.assertEqual(self.paths["app"].name, "MacOSNetPlayer.app")
        self.assertEqual(self.paths["uitests"].name, "MacOSNetPlayerUITests.xctest")
        self.assertEqual(
            self.paths["uitests"].relative_to(self.derived).as_posix(),
            "Build/Products/Debug/MacOSNetPlayerUITests-Runner.app/Contents/PlugIns/MacOSNetPlayerUITests.xctest",
        )
        self.assertEqual(self.paths["runner"].name, "MacOSNetPlayerUITests-Runner.app")
        self.assertTrue(all(path.is_relative_to(self.derived) for path in self.paths.values()))

    def test_derived_data_conflict_is_rejected_and_fixed_path_is_accepted(self) -> None:
        with mock.patch.dict(os.environ, {"DERIVED_DATA_PATH": str(self.root / "other")}, clear=True):
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "conflicts"):
                ui_e2e.local_signed_derived_data_path(self.signing_config)
        with mock.patch.dict(os.environ, {"DERIVED_DATA_PATH": str(self.derived)}, clear=True):
            self.assertEqual(ui_e2e.local_signed_derived_data_path(self.signing_config), self.derived)

    def test_session_lock_rejects_contention_and_releases_without_stale_cleanup(self) -> None:
        lock = ui_e2e.acquire_local_signed_lock(self.session_root)
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "Another local-signed"):
            ui_e2e.acquire_local_signed_lock(self.session_root)
        self.assertTrue(lock.is_dir())
        ui_e2e.release_local_signed_lock(lock)
        self.assertFalse(lock.exists())

    def test_session_lock_is_released_when_the_run_fails(self) -> None:
        lock = self.session_root / ui_e2e.LOCAL_SIGNED_LOCK
        with mock.patch.object(ui_e2e.sys, "platform", "darwin"), \
                mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch.object(ui_e2e.local_build, "load_config", return_value=self.signing_config), \
                mock.patch.object(ui_e2e, "_run_ui_e2e", side_effect=ui_e2e.UIE2EError("fixture failure")):
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "fixture failure"):
                ui_e2e.run_ui_e2e("macos", mode="local-signed")
        self.assertFalse(lock.exists())

    def test_receipt_first_write_reuse_and_fail_closed_identity_drift(self) -> None:
        self.assertIsNone(ui_e2e.read_local_signed_receipt(
            self.session_root,
            signing_config=self.signing_config,
            derived_data_path=self.derived,
        ))
        products = self.product_receipt()
        source = {"repository": str(ui_e2e.ROOT), "head": "a" * 40, "dirty": True}
        with mock.patch.object(ui_e2e.local_build, "_source_binding", return_value=source):
            ui_e2e.write_local_signed_receipt(
                self.session_root,
                derived_data_path=self.derived,
                signing_config=self.signing_config,
                products=products,
                deadline=ui_e2e.time.monotonic() + 10,
            )
        receipt = ui_e2e.read_local_signed_receipt(
            self.session_root,
            signing_config=self.signing_config,
            derived_data_path=self.derived,
        )
        self.assertEqual(receipt["products"], products)
        self.assertEqual(receipt["source"], source)
        receipt_path = self.session_root / ui_e2e.LOCAL_SIGNED_RECEIPT
        invalid = dict(receipt, team_id="OTHERTEAM1")
        receipt_path.write_text(json.dumps(invalid), encoding="utf-8")
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "does not match the pinned profile"):
            ui_e2e.read_local_signed_receipt(
                self.session_root,
                signing_config=self.signing_config,
                derived_data_path=self.derived,
            )
        receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
        historical_root = self.root / "previous-linked-worktree"
        self.assertFalse(historical_root.exists())
        historical = dict(receipt)
        historical["source"] = {
            "repository": str(historical_root),
            "head": "b" * 40,
            "dirty": False,
        }
        receipt_path.write_text(json.dumps(historical), encoding="utf-8")
        reused = ui_e2e.read_local_signed_receipt(
            self.session_root,
            signing_config=self.signing_config,
            derived_data_path=self.derived,
        )
        self.assertEqual(reused["source"], historical["source"])
        for invalid_source in (
            {"repository": "relative/checkout", "head": "b" * 40, "dirty": False},
            {"repository": str(historical_root), "head": "not-a-commit", "dirty": False},
            {"repository": str(historical_root), "head": "b" * 40, "dirty": "false"},
        ):
            with self.subTest(invalid_source=invalid_source):
                invalid = dict(receipt, source=invalid_source)
                receipt_path.write_text(json.dumps(invalid), encoding="utf-8")
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "invalid source binding"):
                    ui_e2e.read_local_signed_receipt(
                        self.session_root,
                        signing_config=self.signing_config,
                        derived_data_path=self.derived,
                    )
        receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
        updated = {name: dict(item) for name, item in products.items()}
        updated["app"]["cdhash"] = "new-cdhash"
        def changed_hash(path: Path, **_kwargs: object) -> dict:
            name = next(name for name, product_path in self.paths.items() if product_path == path)
            return updated[name]

        with mock.patch.object(ui_e2e.local_build, "verify_ui_e2e_bundle", side_effect=changed_hash):
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "CDHash drifted"):
                ui_e2e.verify_local_signed_products(
                    self.paths,
                    signing_config=self.signing_config,
                    previous_receipt=receipt,
                    deadline=ui_e2e.time.monotonic() + 10,
                )
        changed_requirement = {name: dict(item) for name, item in products.items()}
        changed_requirement["app"]["designated_requirement"] = 'identifier "unexpected"'
        def changed_requirement_for(path: Path, **_kwargs: object) -> dict:
            name = next(name for name, product_path in self.paths.items() if product_path == path)
            return changed_requirement[name]

        with mock.patch.object(ui_e2e.local_build, "verify_ui_e2e_bundle", side_effect=changed_requirement_for):
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "identity or CDHash drifted"):
                ui_e2e.verify_local_signed_products(
                    self.paths,
                    signing_config=self.signing_config,
                    previous_receipt=receipt,
                    deadline=ui_e2e.time.monotonic() + 10,
                    compare_codehash=False,
                )

    def test_missing_receipt_with_existing_product_fails_closed(self) -> None:
        self.paths["app"].mkdir(parents=True)
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "receipt is missing"):
            ui_e2e.read_local_signed_receipt(
                self.session_root,
                signing_config=self.signing_config,
                derived_data_path=self.derived,
            )

    def test_local_signed_split_verifies_all_products_before_tests(self) -> None:
        events: list[str] = []

        def run(command: list[str], **_kwargs: object) -> ui_e2e.CommandResult:
            action = command[-1]
            events.append(action)
            if action == "build-for-testing":
                self.make_product_bundles(self.paths)
            return ui_e2e.CommandResult(0, 0)

        def details(path: Path, **_kwargs: object) -> tuple[str, str, str, str, str]:
            name = next(name for name, product_path in self.paths.items() if product_path == path)
            events.append(f"verified:{name}")
            identifier = ui_e2e.local_signed_bundle_identifiers()[name]
            return (
                identifier,
                self.signing_config["team_id"],
                self.signing_config["certificate_sha1"],
                f'identifier "{identifier}" and anchor apple',
                f"{name}-cdhash",
            )

        source = {"repository": str(ui_e2e.ROOT), "head": "a" * 40, "dirty": True}
        with mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch.object(ui_e2e.time, "monotonic", return_value=1000), \
                mock.patch.object(ui_e2e, "run_process", side_effect=run) as run_mock, \
                mock.patch.object(ui_e2e.local_build, "_codesign_details", side_effect=details), \
                mock.patch.object(ui_e2e.local_build, "_source_binding", return_value=source):
            result = ui_e2e.run_local_signed_xcode(
                endpoint="http://127.0.0.1:43210",
                task_endpoint="http://127.0.0.1:43211",
                derived_data_path=self.derived,
                result_bundle=self.root / "result.xcresult",
                destination="platform=macOS",
                logs_dir=self.root,
                child_env={},
                signing_config=self.signing_config,
                session_root=self.session_root,
                deadline=1300,
                log=lambda _message: None,
            )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(events, [
            "build-for-testing",
            "verified:app",
            "verified:uitests",
            "verified:runner",
            "test-without-building",
        ])
        self.assertEqual([call.kwargs["timeout_seconds"] for call in run_mock.call_args_list], [300, 300])
        self.assertTrue((self.session_root / ui_e2e.LOCAL_SIGNED_RECEIPT).is_file())

    def test_existing_total_and_artifact_budgets_remain_fixed(self) -> None:
        self.assertEqual(ui_e2e.TOTAL_RUN_BUDGET_SECONDS, 35 * 60)
        self.assertEqual(ui_e2e.COMMAND_TIMEOUT_SECONDS, 30 * 60)
        self.assertEqual(ui_e2e.MAX_COMMAND_LOG_BYTES, 8 * 1024 * 1024)
        self.assertEqual(ui_e2e.MAX_ARTIFACT_BYTES, 32 * 1024 * 1024)
        self.assertEqual(ui_e2e.MAX_RESULT_BUNDLE_BYTES, 20 * 1024 * 1024)

    def test_build_or_signature_drift_prevents_test_execution(self) -> None:
        scenarios = ("build-failure", "team", "certificate", "identifier", "requirement")
        for scenario in scenarios:
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                config = dict(self.signing_config, durable_root=root / "LocalBuild")
                session_root = ui_e2e.local_signed_session_root(config)
                session_root.mkdir(parents=True)
                derived = session_root / "DerivedData"
                paths = ui_e2e.local_signed_product_paths(derived)
                identifiers = ui_e2e.local_signed_bundle_identifiers()
                events: list[str] = []
                built = False
                previous = None
                if scenario == "requirement":
                    self.make_product_bundles(paths)
                    previous = {
                        "products": {
                            name: {
                                "path": str(path),
                                "identifier": identifiers[name],
                                "team_id": config["team_id"],
                                "certificate_sha1": config["certificate_sha1"],
                                "designated_requirement": f'identifier "{identifiers[name]}" and anchor apple',
                                "cdhash": f"{name}-cdhash",
                            }
                            for name, path in paths.items()
                        }
                    }

                def run(command: list[str], **_kwargs: object) -> ui_e2e.CommandResult:
                    nonlocal built
                    action = command[-1]
                    events.append(action)
                    if action == "build-for-testing":
                        if scenario == "build-failure":
                            return ui_e2e.CommandResult(65, 0)
                        if previous is None:
                            self.make_product_bundles(paths)
                        built = True
                    return ui_e2e.CommandResult(0, 0)

                def details(path: Path, **_kwargs: object) -> tuple[str, str, str, str, str]:
                    name = next(name for name, product_path in paths.items() if product_path == path)
                    identifier = identifiers[name]
                    team = config["team_id"]
                    certificate = config["certificate_sha1"]
                    requirement = f'identifier "{identifier}" and anchor apple'
                    cdhash = f"{name}-cdhash"
                    if scenario == "requirement" and built:
                        requirement = 'identifier "changed" and anchor apple'
                        cdhash = "changed-cdhash"
                    elif scenario == "team" and name == "app":
                        team = "OTHERTEAM1"
                    elif scenario == "certificate" and name == "app":
                        certificate = "B" * 40
                    elif scenario == "identifier" and name == "app":
                        identifier = "unexpected.bundle.id"
                    return identifier, team, certificate, requirement, cdhash

                failure = scenario != "build-failure"
                with mock.patch.dict(os.environ, {}, clear=True), \
                        mock.patch.object(ui_e2e, "run_process", side_effect=run), \
                        mock.patch.object(ui_e2e, "read_local_signed_receipt", return_value=previous), \
                        mock.patch.object(ui_e2e.local_build, "_codesign_details", side_effect=details), \
                        mock.patch.object(ui_e2e.local_build, "_source_binding", return_value={
                            "repository": str(ui_e2e.ROOT), "head": "a" * 40, "dirty": True,
                        }):
                    if failure:
                        with self.assertRaises(ui_e2e.UIE2EError):
                            ui_e2e.run_local_signed_xcode(
                                endpoint="http://127.0.0.1:43210",
                                task_endpoint="http://127.0.0.1:43211",
                                derived_data_path=derived,
                                result_bundle=root / "result.xcresult",
                                destination="platform=macOS",
                                logs_dir=root,
                                child_env={},
                                signing_config=config,
                                session_root=session_root,
                                deadline=ui_e2e.time.monotonic() + 300,
                                log=lambda _message: None,
                            )
                    else:
                        with self.assertRaisesRegex(ui_e2e.UIE2EError, "exited 65"):
                            ui_e2e.run_local_signed_xcode(
                                endpoint="http://127.0.0.1:43210",
                                task_endpoint="http://127.0.0.1:43211",
                                derived_data_path=derived,
                                result_bundle=root / "result.xcresult",
                                destination="platform=macOS",
                                logs_dir=root,
                                child_env={},
                                signing_config=config,
                                session_root=session_root,
                                deadline=ui_e2e.time.monotonic() + 300,
                                log=lambda _message: None,
                            )
                self.assertNotIn("test-without-building", events)
                self.assertEqual(events[0], "build-for-testing")


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
        self.assertNotIn("-quiet", command)
        self.assertIn(f"APP_BUNDLE_IDENTIFIER={bundle_id}", command)
        self.assertIn("UI_E2E_SERVER_ADDRESS=http://127.0.0.1:43210", command)
        self.assertEqual(
            sum(argument.startswith("APP_BUNDLE_IDENTIFIER=") for argument in command),
            1,
        )

    def test_xcodebuild_passes_task_fixture_endpoint_when_configured(self) -> None:
        command = ui_e2e.build_xcodebuild_command(
            platform="macos",
            server_address="http://127.0.0.1:43210",
            task_server_address="http://127.0.0.1:43211",
            app_bundle_identifier="dev.joey.tvos-net-player.uie2e." + "b" * 32,
            derived_data_path=Path("/tmp/DerivedData"),
            result_bundle_path=Path("/tmp/UIE2E.xcresult"),
            destination="platform=macOS",
        )
        self.assertIn("UI_E2E_SERVER_ADDRESS=http://127.0.0.1:43210", command)
        self.assertIn("UI_E2E_TASK_SERVER_ADDRESS=http://127.0.0.1:43211", command)

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

    def test_local_signed_xcodebuild_uses_fixed_ids_and_split_actions(self) -> None:
        signing = {"identity_sha1": "A" * 40, "team_id": "TESTTEAM01"}
        derived = Path("/stable/LocalBuild/UIE2E/DerivedData")
        result_bundle = Path("/private/tmp/UIE2E.xcresult")
        common = {
            "platform": "macos",
            "server_address": "http://127.0.0.1:43210",
            "task_server_address": "http://127.0.0.1:43211",
            "app_bundle_identifier": ui_e2e.LOCAL_SIGNED_BUNDLE_ID,
            "derived_data_path": derived,
            "result_bundle_path": result_bundle,
            "destination": "platform=macOS",
            "mode": "local-signed",
            "signing_config": signing,
        }
        build = ui_e2e.build_xcodebuild_command(**common, action="build-for-testing")
        self.assertEqual(build[-1], "build-for-testing")
        self.assertNotIn("-resultBundlePath", build)
        self.assertIn(f"-derivedDataPath", build)
        self.assertIn(str(derived), build)
        settings = dict(argument.split("=", 1) for argument in build if "=" in argument and not argument.startswith("-"))
        self.assertEqual(settings["CODE_SIGN_IDENTITY"], signing["identity_sha1"])
        self.assertEqual(settings["DEVELOPMENT_TEAM"], signing["team_id"])
        self.assertEqual(settings["CODE_SIGNING_REQUIRED"], "YES")
        self.assertEqual(settings["APP_BUNDLE_IDENTIFIER"], ui_e2e.LOCAL_SIGNED_BUNDLE_ID)

        test = ui_e2e.build_xcodebuild_command(**common, action="test-without-building")
        self.assertEqual(test[-1], "test-without-building")
        result_index = test.index("-resultBundlePath")
        self.assertEqual(test[result_index + 1], str(result_bundle))
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "fixed macOS"):
            ui_e2e.build_xcodebuild_command(
                **dict(common, platform="tvos"), action="build-for-testing"
            )

    def test_scheme_requires_test_action_endpoint_mapping(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scheme = Path(directory) / "UIE2E.xcscheme"
            scheme.write_text(
                '<Scheme><TestAction shouldUseLaunchSchemeArgsEnv="NO">'
                '<EnvironmentVariables><EnvironmentVariable '
                'key="UI_E2E_SERVER_ADDRESS" '
                'value="$(UI_E2E_SERVER_ADDRESS)" isEnabled="YES" />'
                '<EnvironmentVariable key="UI_E2E_TASK_SERVER_ADDRESS" '
                'value="$(UI_E2E_TASK_SERVER_ADDRESS)" isEnabled="YES" />'
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
                '<EnvironmentVariable key="UI_E2E_TASK_SERVER_ADDRESS" '
                'value="$(UI_E2E_TASK_SERVER_ADDRESS)" isEnabled="YES" />'
                '</EnvironmentVariables><Testables><TestableReference skipped="YES">'
                '<BuildableReference BlueprintName="ExampleUITests" />'
                '</TestableReference></Testables></TestAction></Scheme>',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "does not test"):
                ui_e2e.verify_scheme_contract(scheme, "ExampleUITests")

    def test_shared_ui_e2e_schemes_map_both_loopback_services(self) -> None:
        expected_targets = {
            "MacOSNetPlayerUIE2E": "MacOSNetPlayerUITests",
            "TVOSNetPlayerUIE2E": "TVOSNetPlayerUITests",
        }
        for scheme_name, target in expected_targets.items():
            with self.subTest(scheme=scheme_name):
                ui_e2e.verify_scheme_contract(
                    ui_e2e.ROOT
                    / "TVOSNetPlayer.xcodeproj"
                    / "xcshareddata"
                    / "xcschemes"
                    / f"{scheme_name}.xcscheme",
                    target,
                )

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
                    "nodeType": "UI test bundle",
                    "name": "MacOSNetPlayerUITests",
                    "children": [
                        {"nodeType": "Test Case", "name": "testPlayback", "result": "Passed"}
                    ],
                }
            ]
        }

    def test_platform_case_matrix_requires_five_cases_without_dropping_existing_coverage(self) -> None:
        task_watch_cases = (
            "testAutomaticTaskWatchRecoversWithoutResubmission",
            "testManualTaskWatchReconnectPreservesPlayback",
            "testSilentTaskWatchReconcilesWithoutInterruptingPlayback",
            "testRelaunchRestoresTaskAndExplicitPlaybackPosition",
        )
        self.assertEqual(ui_e2e.EXPECTED_UI_CASES, task_watch_cases)
        self.assertEqual(
            ui_e2e.EXPECTED_CASES_BY_PLATFORM["macos"],
            task_watch_cases + ("testConnectBrowseAndControlRealPlayback",),
        )
        self.assertEqual(
            ui_e2e.EXPECTED_CASES_BY_PLATFORM["tvos"],
            task_watch_cases + ("testRemoteConnectPageSelectAndPlayRealFixture",),
        )

    def test_documented_ui_bundle_with_case_attachments_counts_execution(self) -> None:
        case = self.tests["testNodes"][0]["children"][0]
        case["children"] = [{"nodeType": "Attachment", "name": "Screenshot"}]
        counts = ui_e2e.validate_xcresult(self.summary, self.tests, "MacOSNetPlayerUITests")
        self.assertEqual(counts["target_passed"], 1)

    def test_ui_bundle_identifier_is_exact_and_unrelated_types_are_rejected(self) -> None:
        bundle = self.tests["testNodes"][0]
        bundle["name"] = "UI tests"
        bundle["nodeIdentifier"] = "/products/MacOSNetPlayerUITests.xctest"
        self.assertEqual(
            ui_e2e.validate_xcresult(self.summary, self.tests, "MacOSNetPlayerUITests")["target_passed"],
            1,
        )
        for node_type in ("Unit test bundle", "Test Target", "Other UI test bundle"):
            with self.subTest(node_type=node_type):
                bundle["nodeType"] = node_type
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "does not contain expected"):
                    ui_e2e.validate_xcresult(self.summary, self.tests, "MacOSNetPlayerUITests")

    def test_failed_case_with_failure_message_child_is_not_a_pass(self) -> None:
        case = self.tests["testNodes"][0]["children"][0]
        case["result"] = "Failed"
        case["children"] = [{"nodeType": "Failure Message", "name": "XCTAssertTrue failed"}]
        with self.assertRaises(ui_e2e.UIE2EError):
            ui_e2e.validate_xcresult(self.summary, self.tests, "MacOSNetPlayerUITests")

    def test_skipped_case_run_cannot_hide_under_passed_parent(self) -> None:
        case = self.tests["testNodes"][0]["children"][0]
        case["children"] = [
            {"nodeType": "Test Case Run", "name": "run 1", "result": "Passed"},
            {"nodeType": "Test Case Run", "name": "run 2", "result": "Skipped"},
        ]
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "skipped UI tests"):
            ui_e2e.validate_xcresult(self.summary, self.tests, "MacOSNetPlayerUITests")

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

    def test_runner_requires_each_named_task_watch_case(self) -> None:
        expected = ui_e2e.EXPECTED_CASES_BY_PLATFORM["macos"]
        self.assertEqual(
            expected,
            (
                "testAutomaticTaskWatchRecoversWithoutResubmission",
                "testManualTaskWatchReconnectPreservesPlayback",
                "testSilentTaskWatchReconcilesWithoutInterruptingPlayback",
                "testRelaunchRestoresTaskAndExplicitPlaybackPosition",
                "testConnectBrowseAndControlRealPlayback",
            ),
        )
        cases = [
            {"nodeType": "Test Case", "name": name, "result": "Passed"}
            for name in expected
        ]
        summary = dict(self.summary, totalTestCount=len(cases), passedTests=len(cases))
        tests = {
            "testNodes": [{
                "nodeType": "UI test bundle",
                "name": "MacOSNetPlayerUITests",
                "children": cases,
            }]
        }
        counts = ui_e2e.validate_xcresult(
            summary, tests, "MacOSNetPlayerUITests", expected
        )
        self.assertEqual(counts["target_passed"], len(expected))
        for missing in expected:
            incomplete = json.loads(json.dumps(tests))
            incomplete["testNodes"][0]["children"] = [
                case for case in incomplete["testNodes"][0]["children"]
                if case["name"] != missing
            ]
            with self.subTest(missing=missing), self.assertRaisesRegex(
                ui_e2e.UIE2EError, "did not execute and pass required UI case"
            ):
                ui_e2e.validate_xcresult(
                    summary, incomplete, "MacOSNetPlayerUITests", expected
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


class TaskFixtureOrchestrationTests(unittest.TestCase):
    def _metrics(self) -> dict[str, object]:
        return {
            "schema_version": 1,
            "scenarios": {
                name: {
                    "task_id": ui_e2e.EXPECTED_TASK_IDS[name],
                    "start_resolution_count": 1,
                    "create_count": 1,
                    "watch_count": 1 if name == "silent" else 2,
                    "watch_stream_open_count": 1 if name == "silent" else 2,
                    "get_task_count": 2 if name == "silent" else 1,
                    "unchanged_get_task_count": 1 if name == "silent" else 0,
                    "completed_get_task_count": 1 if name == "silent" else 0,
                    "output_count": 2 if name == "silent" else 1,
                    "cancel_count": 0,
                    "playback_source_request_count": 2 if name == "resumption" else 0,
                    "playback_source_requests": (
                        [ui_e2e.EXPECTED_PLAYBACK_SOURCES["resumption"]] * 2
                        if name == "resumption"
                        else []
                    ),
                    "rpc_trace": ["watch", "get_task", "get_task"] if name == "silent" else [],
                    **(
                        {"manual_resume_read_before_watch": True}
                        if name == "manual"
                        else {}
                    ),
                }
                for name in ("automatic", "manual", "silent", "resumption")
            },
        }

    def test_fixture_command_requires_loopback_mp4_route(self) -> None:
        media_url = ui_e2e.build_rust_fixture_media_url(43211)
        command = ui_e2e.build_task_fixture_command(
            Path("/build/task-watch-ui-fixture"),
            ready_file=Path("/tmp/ready.json"),
            metrics_file=Path("/tmp/metrics.json"),
            media_url=media_url,
        )
        self.assertEqual(command[-2:], ["--media-url", media_url])
        for invalid in (
            "https://127.0.0.1:43211/media/example/original",
            "http://127.0.0.1:43211/media/example/original?token=secret",
            "http://example.test:43211/media/example/original",
        ):
            with self.subTest(invalid=invalid), self.assertRaises(ui_e2e.UIE2EError):
                ui_e2e.validate_media_fixture_url(invalid)

    def test_fixture_build_uses_bounded_debug_product_and_rejects_incomplete_builds(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / ui_e2e.TASK_FIXTURE_PRODUCT
            binary.touch(mode=0o700)
            with (
                mock.patch.object(
                    ui_e2e,
                    "run_process",
                    return_value=ui_e2e.CommandResult(0, 10),
                ) as run_process,
                mock.patch.object(ui_e2e, "resolve_swift_bin_path", return_value=Path(directory)),
            ):
                result = ui_e2e.build_task_fixture(Path(directory), timeout_seconds=60)
            self.assertEqual(result, binary)
            args = run_process.call_args
            self.assertEqual(args.args[0], ui_e2e.build_task_fixture_swift_command())
            self.assertEqual(args.kwargs["timeout_seconds"], 60)
            self.assertEqual(args.kwargs["cwd"], ui_e2e.ROOT)
            self.assertEqual(args.kwargs["progress_path"], Path(directory) / "ui-progress.jsonl")
            self.assertEqual(args.kwargs["progress_phase"], "fixture_build")

            for incomplete in (
                ui_e2e.CommandResult(0, 10, timed_out=True),
                ui_e2e.CommandResult(0, 10, output_limited=True),
            ):
                with (
                    mock.patch.object(ui_e2e, "run_process", return_value=incomplete),
                    self.assertRaises(ui_e2e.UIE2EError),
                ):
                    ui_e2e.build_task_fixture(Path(directory), timeout_seconds=60)

    def test_swift_bin_path_capture_call_matches_real_wrapper_signature(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logs = root / "logs"
            bin_path = root / ".build" / "debug"
            with (
                mock.patch.object(ui_e2e, "ROOT", root),
                mock.patch.object(
                    ui_e2e, "run_capture", autospec=True, return_value=f"{bin_path}\n"
                ) as capture,
            ):
                self.assertEqual(ui_e2e.resolve_swift_bin_path(logs), bin_path.resolve())
            capture.assert_called_once_with(
                ["swift", "build", "--show-bin-path", "--disable-automatic-resolution"],
                logs_dir=logs,
                name="task-fixture-bin-path",
                timeout_seconds=30,
                max_output_bytes=16 * 1024,
                cwd=root,
            )

    def test_fixture_ready_file_is_bounded_and_returns_grpc_endpoint(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            ready = Path(directory) / "ready.json"
            ready.write_text(
                '{"schema_version":1,"server_address":"127.0.0.1:43211"}',
                encoding="utf-8",
            )
            process = mock.Mock(output_limited=False)
            process.is_running.return_value = True
            self.assertEqual(
                ui_e2e.wait_for_task_fixture(process, ready),
                "http://127.0.0.1:43211",
            )
            ready.write_text("{}", encoding="utf-8")
            with (
                mock.patch.object(ui_e2e.time, "monotonic", side_effect=[0.0, 1.0]),
                mock.patch.object(ui_e2e.time, "sleep"),
                self.assertRaisesRegex(ui_e2e.UIE2EError, "readiness timed out"),
            ):
                ui_e2e.wait_for_task_fixture(process, ready, timeout_seconds=0.5)

    def test_rust_media_fixture_head_probe_is_finite_and_checks_real_mp4(self) -> None:
        response = mock.MagicMock()
        response.status = 200
        response.headers.get_content_type.return_value = "video/mp4"
        response.headers.get.return_value = "4096"
        response.__enter__.return_value = response
        opener = mock.Mock()
        opener.open.return_value = response
        with mock.patch.object(ui_e2e.urllib.request, "build_opener", return_value=opener):
            ui_e2e.verify_media_fixture_url(
                ui_e2e.build_rust_fixture_media_url(43211), 4096
            )
        request = opener.open.call_args.args[0]
        self.assertEqual(request.get_method(), "HEAD")
        self.assertEqual(opener.open.call_args.kwargs["timeout"], 3)

        response.status = 404
        with (
            mock.patch.object(ui_e2e.urllib.request, "build_opener", return_value=opener),
            self.assertRaisesRegex(ui_e2e.UIE2EError, "expected MP4 HEAD response"),
        ):
            ui_e2e.verify_media_fixture_url(
                ui_e2e.build_rust_fixture_media_url(43211), 4096
            )

    def test_fixture_readiness_fails_on_output_limit_or_early_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            ready = Path(directory) / "ready.json"
            process = mock.Mock(output_limited=True)
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "bounded log"):
                ui_e2e.wait_for_task_fixture(process, ready)
            process.output_limited = False
            process.is_running.return_value = False
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "exited before becoming ready"):
                ui_e2e.wait_for_task_fixture(process, ready)

    def test_fixture_metrics_accept_success_and_reject_bad_counts_and_resume_proof(self) -> None:
        metrics = self._metrics()
        validated = ui_e2e.validate_task_fixture_metrics(metrics)
        self.assertEqual(validated["scenarios"]["automatic"]["create_count"], 1)
        bad = json.loads(json.dumps(metrics))
        bad["scenarios"]["manual"]["create_count"] = 2
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "exactly one task"):
            ui_e2e.validate_task_fixture_metrics(bad)
        bad = json.loads(json.dumps(metrics))
        bad["scenarios"]["manual"]["manual_resume_read_before_watch"] = False
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "before its final watch"):
            ui_e2e.validate_task_fixture_metrics(bad)

    def test_fixture_metrics_require_exact_resolution_and_saved_playback_source(self) -> None:
        metrics = self._metrics()
        self.assertEqual(
            ui_e2e.validate_task_fixture_metrics(metrics)["scenarios"]["resumption"][
                "playback_source_requests"
            ],
            [ui_e2e.EXPECTED_PLAYBACK_SOURCES["resumption"]] * 2,
        )

        invalid_cases = (
            ("start_resolution_count", 2, "exactly one resolution"),
            ("start_resolution_count", 0, "exactly one resolution"),
            ("playback_source_request_count", 1, "exactly 2 playback source request"),
            ("playback_source_request_count", 3, "exactly 2 playback source request"),
            (
                "playback_source_requests",
                [{"item_id": "ui-e2e-resumption-decoy-library-item", "variant_id": "ui-e2e-resumption-mp4"}],
                "unexpected playback source",
            ),
            (
                "playback_source_requests",
                [{"item_id": "ui-e2e-resumption-library-item", "variant_id": "ui-e2e-resumption-decoy-mp4"}],
                "unexpected playback source",
            ),
        )
        for key, value, message in invalid_cases:
            with self.subTest(key=key, value=value):
                bad = self._metrics()
                bad["scenarios"]["resumption"][key] = value
                with self.assertRaisesRegex(ui_e2e.UIE2EError, message):
                    ui_e2e.validate_task_fixture_metrics(bad)

        bad = self._metrics()
        bad["scenarios"]["automatic"]["start_resolution_count"] = True
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "exactly one resolution"):
            ui_e2e.validate_task_fixture_metrics(bad)

        bad = self._metrics()
        bad["scenarios"]["manual"]["playback_source_request_count"] = 1
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "exactly 0 playback source request"):
            ui_e2e.validate_task_fixture_metrics(bad)

        bad = self._metrics()
        bad["scenarios"]["silent"]["playback_source_requests"] = [{"item_id": "unexpected"}]
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "unexpected playback source"):
            ui_e2e.validate_task_fixture_metrics(bad)

        bad = self._metrics()
        bad["scenarios"]["unexpected"] = {}
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "unexpected scenario"):
            ui_e2e.validate_task_fixture_metrics(bad)

    def test_fixture_metrics_require_expected_task_ids_and_positive_io(self) -> None:
        metrics = self._metrics()
        metrics["scenarios"]["automatic"]["task_id"] = "unexpected"
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "unexpected task ID"):
            ui_e2e.validate_task_fixture_metrics(metrics)
        metrics = self._metrics()
        metrics["scenarios"]["manual"]["output_count"] = 0
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "positive output_count"):
            ui_e2e.validate_task_fixture_metrics(metrics)

    def test_fixture_metrics_require_silent_watch_reconciliation_evidence(self) -> None:
        metrics = self._metrics()
        self.assertIn("silent", ui_e2e.validate_task_fixture_metrics(metrics)["scenarios"])

        missing = json.loads(json.dumps(metrics))
        del missing["scenarios"]["silent"]
        with self.assertRaisesRegex(ui_e2e.UIE2EError, "missing the silent scenario"):
            ui_e2e.validate_task_fixture_metrics(missing)

        invalid_values = (
            ("watch_count", 2, "exactly one WatchTasks"),
            ("watch_stream_open_count", 0, "stream opened"),
            ("get_task_count", 1, "at least 2 get_task_count"),
            ("unchanged_get_task_count", 0, "at least 1 unchanged_get_task_count"),
            ("completed_get_task_count", 0, "at least 1 completed_get_task_count"),
            ("output_count", 1, "at least 2 output_count"),
            ("rpc_trace", ["get_task", "watch"], "after opening WatchTasks"),
        )
        for key, value, message in invalid_values:
            with self.subTest(key=key):
                bad = json.loads(json.dumps(metrics))
                bad["scenarios"]["silent"][key] = value
                with self.assertRaisesRegex(ui_e2e.UIE2EError, message):
                    ui_e2e.validate_task_fixture_metrics(bad)

    def test_fixture_metrics_reject_cancellation_side_effects(self) -> None:
        for scenario in ("automatic", "manual", "silent", "resumption"):
            for cancel_count in (1, -1, True, "0", None):
                with self.subTest(scenario=scenario, cancel_count=cancel_count):
                    metrics = self._metrics()
                    metrics["scenarios"][scenario]["cancel_count"] = cancel_count
                    with self.assertRaisesRegex(ui_e2e.UIE2EError, "must not cancel server work"):
                        ui_e2e.validate_task_fixture_metrics(metrics)

    def test_fixture_cleanup_runs_after_test_failure(self) -> None:
        stopped: list[str] = []
        cleanup = ui_e2e.CleanupStack()
        cleanup.defer("owned Swift task fixture", lambda: stopped.append("fixture"))
        with self.assertRaisesRegex(RuntimeError, "xcode failure"):
            with cleanup:
                raise RuntimeError("xcode failure")
        self.assertEqual(stopped, ["fixture"])

class FailedXcodeDiagnosticsTests(unittest.TestCase):
    def _payloads(self) -> dict[str, object]:
        return {
            "summary": {"totalTestCount": 1, "passedTests": 0, "failedTests": 1, "skippedTests": 0},
            "tests": {"testNodes": [{
                "nodeType": "UI test bundle", "name": "MacOSNetPlayerUITests",
                "children": [{
                    "nodeType": "Test Case", "name": "testConnectBrowseAndControlRealPlayback()",
                    "nodeIdentifier": "UIPlaybackTests/testConnectBrowseAndControlRealPlayback()",
                    "result": "Failed",
                    "children": [{"nodeType": "Failure Message", "name": "XCTAssertTrue failed - cache row missing"}],
                }],
            }]},
            "test-details": {"testRuns": [{"nodeType": "Failure Message", "name": "UI control was not found"}]},
            "activities": {"activities": [{
                "title": "Find Cache tab", "isAssociatedWithFailure": True,
                "attachments": [{"name": "Screenshot", "payloadId": "fixture-payload", "uuid": "fixture-attachment"}],
            }]},
        }

    def test_failed_exit_retains_bounded_summary_details_and_primary_reason(self) -> None:
        payloads = self._payloads()
        payloads["summary"]["message"] = "ordinary diagnostic detail"
        messages: list[str] = []
        activities_available = True

        def capture(command: list[str], **kwargs: object) -> object:
            if command[4] == "activities" and not activities_available:
                raise ui_e2e.UIE2EError("activities extraction fixture failure")
            kwargs["log_path"].write_text(json.dumps(payloads[command[4]]), encoding="utf-8")
            self.assertLessEqual(kwargs["timeout_seconds"], 120)
            self.assertLessEqual(kwargs["max_output_bytes"], ui_e2e.MAX_RESULT_SUMMARY_BYTES)
            return ui_e2e.CommandResult(0, 1)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "UIE2E.xcresult"
            bundle.mkdir()
            with mock.patch.object(ui_e2e, "run_process", side_effect=capture) as run:
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "xcodebuild test exited 65"):
                    ui_e2e.finish_xcode_run(
                        ui_e2e.CommandResult(65, 1), result_bundle=bundle, logs_dir=root,
                        expected_target="MacOSNetPlayerUITests", log=messages.append,
                    )
            self.assertEqual([call.args[0][4] for call in run.call_args_list],
                             ["summary", "tests", "test-details", "activities"])
            self.assertTrue((root / "xcresult-summary.json").is_file())
            self.assertTrue((root / "xcresult-tests.json").is_file())
            self.assertTrue((root / "xcresult-failure-details.json").is_file())
            self.assertTrue((root / "xcresult-failure-activities.json").is_file())
            self.assertFalse((root / "result-validation.json").exists())
            self.assertTrue(any("cache row missing" in message for message in messages))
            self.assertTrue(any("no eligible named teardown AX/screenshot" in message for message in messages))
            self.assertTrue((root / "xcresult-attachment-export.json").is_file())
            self.assertIn("xcresult diagnostic: ordinary diagnostic detail", messages)
            self.assertFalse(any(message.startswith("xcresult failure:") for message in messages))

        activities_available = False
        messages.clear()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "UIE2E.xcresult"
            bundle.mkdir()
            with mock.patch.object(ui_e2e, "run_process", side_effect=capture):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "xcodebuild test exited 65"):
                    ui_e2e.finish_xcode_run(
                        ui_e2e.CommandResult(65, 1), result_bundle=bundle, logs_dir=root,
                        expected_target="MacOSNetPlayerUITests", log=messages.append,
                    )
            self.assertTrue(any("activities extraction fixture failure" in message for message in messages))
            self.assertTrue(any("no eligible named teardown AX/screenshot" in message for message in messages))
            self.assertFalse((root / "xcresult-failure-activities.json").exists())

    def test_extraction_error_never_replaces_failed_xcode_exit(self) -> None:
        calls: list[str] = []
        messages: list[str] = []

        def capture(command: list[str], **kwargs: object) -> object:
            calls.append(command[4])
            if command[4] == "summary":
                raise ui_e2e.UIE2EError("summary extraction fixture failure")
            kwargs["log_path"].write_text('{"testNodes": []}', encoding="utf-8")
            return ui_e2e.CommandResult(0, 1)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "UIE2E.xcresult"
            bundle.mkdir()
            with mock.patch.object(ui_e2e, "run_process", side_effect=capture):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "xcodebuild test exited 65"):
                    ui_e2e.finish_xcode_run(
                        ui_e2e.CommandResult(65, 1), result_bundle=bundle, logs_dir=root,
                        expected_target="MacOSNetPlayerUITests", log=messages.append,
                    )
            self.assertEqual(calls, ["summary", "tests"])
            self.assertTrue(any("summary extraction fixture failure" in message for message in messages))

    def test_failed_xcode_cannot_pass_even_with_passing_result_payloads(self) -> None:
        payloads = {
            "summary": {"totalTestCount": 1, "passedTests": 1, "failedTests": 0, "skippedTests": 0},
            "tests": {"testNodes": [{
                "nodeType": "UI test bundle", "name": "MacOSNetPlayerUITests",
                "children": [{"nodeType": "Test Case", "name": "testPlayback", "result": "Passed"}],
            }]},
        }

        def capture(command: list[str], **kwargs: object) -> object:
            kwargs["log_path"].write_text(json.dumps(payloads[command[4]]), encoding="utf-8")
            return ui_e2e.CommandResult(0, 1)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "UIE2E.xcresult"
            bundle.mkdir()
            with mock.patch.object(ui_e2e, "run_process", side_effect=capture):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "xcodebuild test exited 65"):
                    ui_e2e.finish_xcode_run(
                        ui_e2e.CommandResult(65, 1), result_bundle=bundle, logs_dir=root,
                        expected_target="MacOSNetPlayerUITests", log=lambda _message: None,
                    )

    def test_successful_xcode_with_missing_diagnostics_still_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "UIE2E.xcresult"
            bundle.mkdir()
            with mock.patch.object(ui_e2e, "run_process", side_effect=ui_e2e.UIE2EError("fixture failure")):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "Required xcresult diagnostics failed"):
                    ui_e2e.finish_xcode_run(
                        ui_e2e.CommandResult(0, 1), result_bundle=bundle, logs_dir=root,
                        expected_target="MacOSNetPlayerUITests", log=lambda _message: None,
                    )

    def test_oversized_bundle_drop_preserves_failure_text_and_omission_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work = root / "work"
            logs = work / "logs"
            logs.mkdir(parents=True)
            (logs / "runner.log").write_text("xcodebuild exited 65", encoding="utf-8")
            (logs / "xcresult-summary.json").write_text('{"message":"cache row missing"}', encoding="utf-8")
            bundle = work / "UIE2E.xcresult"
            bundle.mkdir()
            (bundle / "data").write_bytes(b"x" * 128)
            stager = ui_e2e.ArtifactStager(root / "artifacts", max_bytes=2048)
            cleanup = ui_e2e.CleanupStack()
            ui_e2e.defer_workspace_cleanup(cleanup, stager, work_dir=work, result_bundle=bundle)
            with mock.patch.object(ui_e2e, "MAX_RESULT_BUNDLE_BYTES", 64), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "exited 65"):
                    with cleanup:
                        raise ui_e2e.UIE2EError("xcodebuild exited 65")
            self.assertFalse(work.exists())
            self.assertFalse((stager.destination / "result.xcresult").exists())
            self.assertIn("cache row missing", (stager.destination / "xcresult-summary.json").read_text())
            self.assertIn("result.xcresult omitted", (stager.destination / "artifact-staging.log").read_text())
            self.assertLessEqual(stager.used_bytes, 2048)

    def test_bundle_staging_error_does_not_replace_primary_test_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work = root / "work"
            logs = work / "logs"
            logs.mkdir(parents=True)
            (logs / "runner.log").write_text("xcodebuild exited 65", encoding="utf-8")
            bundle = work / "UIE2E.xcresult"
            bundle.mkdir()
            (bundle / "unsafe-link").symlink_to(root)
            stager = ui_e2e.ArtifactStager(root / "artifacts")
            cleanup = ui_e2e.CleanupStack()
            ui_e2e.defer_workspace_cleanup(cleanup, stager, work_dir=work, result_bundle=bundle)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "xcodebuild exited 65"):
                    with cleanup:
                        raise ui_e2e.UIE2EError("xcodebuild exited 65")
            self.assertFalse(work.exists())
            self.assertIn("staging failed", (stager.destination / "artifact-staging.log").read_text())


class FailureAttachmentExportTests(unittest.TestCase):
    @staticmethod
    def activities() -> dict[str, object]:
        # Xcode 26 activities schema: repeated attachments have immutable payloadId.
        screenshot = {
            "lifetime": "keepAlways",
            "name": "Failure screenshot_0_37B8A5AC-F0CD-4FBE-B5C6-5A48ED58FEAE.png",
            "payloadId": "0~screenshot-fixture==",
            "timestamp": 1792683962.115,
            "uuid": "4F6042E7-21E8-4AAD-83A8-E77085689EE8",
        }
        ax = {
            "lifetime": "keepAlways",
            "name": "Failure accessibility tree_0_0A07D7A0-A499-4387-9789-7EB80C24F684.txt",
            "payloadId": "0~ax-fixture==",
            "timestamp": 1792683962.506,
            "uuid": "5494B0A0-2656-4DB5-8EAD-5DAA35070440",
        }
        return {
            "testRuns": [{
                "activities": [
                    {"title": "Start Test", "attachments": [{
                        "lifetime": "deleteOnSuccess",
                        "name": "Screen Recording.mp4",
                        "payloadId": "0~recording-fixture==",
                        "timestamp": 1792683952.93,
                        "uuid": "0A239948-2F98-492B-9EC7-7EAFB094B660",
                    }]},
                    {
                        "title": "Tear Down",
                        "attachments": [screenshot, ax],
                        "childActivities": [
                            {
                                "title": "Added attachment named 'Failure screenshot'",
                                "attachments": [dict(screenshot)],
                            },
                            {
                                "title": "Added attachment named 'Failure accessibility tree'",
                                "attachments": [dict(ax)],
                            },
                        ],
                    },
                ],
            }],
        }

    def test_actual_schema_selects_two_named_teardown_payloads_not_recordings(self) -> None:
        selected = ui_e2e._selected_failure_attachments(self.activities())
        self.assertEqual(
            [item["payloadId"] for item in selected],
            ["0~ax-fixture==", "0~screenshot-fixture=="],
        )
        altered = self.activities()
        teardown = altered["testRuns"][0]["activities"][1]
        teardown["attachments"][0]["payloadId"] = "../unsafe"
        teardown["childActivities"] = []
        self.assertEqual(len(ui_e2e._selected_failure_attachments(altered)), 1)
        teardown["title"] = "Set Up"
        self.assertEqual(ui_e2e._selected_failure_attachments(altered), [])

    def test_selected_export_uses_id_fixed_paths_and_per_payload_limits(self) -> None:
        calls: list[list[str]] = []

        def capture(command: list[str], **kwargs: object) -> object:
            calls.append(command)
            payload_id = command[command.index("--id") + 1]
            output = Path(command[command.index("--output-path") + 1])
            self.assertEqual(output.name, "payload")
            self.assertNotIn("attachments", command)
            self.assertIn("--legacy", command)
            expected_limit = (
                512 * 1024 if payload_id == "0~ax-fixture==" else 2 * 1024 * 1024
            )
            self.assertEqual(int(command[3]), expected_limit)
            self.assertLessEqual(kwargs["timeout_seconds"], 10)
            self.assertEqual(kwargs["max_output_bytes"], 16 * 1024)
            kwargs["log_path"].write_text("exported", encoding="utf-8")
            output.write_bytes(
                b"Application tree\n"
                if "ax-fixture" in payload_id else b"\x89PNG\r\n\x1a\nfixture"
            )
            return ui_e2e.CommandResult(0, 8)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(ui_e2e, "run_process", side_effect=capture):
                ui_e2e.export_failure_attachments(
                    self.activities(),
                    result_bundle=root / "test.xcresult",
                    logs_dir=root,
                    deadline=ui_e2e.time.monotonic() + 10,
                    log=lambda _message: None,
                )
            self.assertEqual(len(calls), 2)
            receipt = json.loads((root / "xcresult-attachment-export.json").read_text())
            self.assertEqual(
                [item["status"] for item in receipt["attachments"]],
                ["exported", "exported"],
            )
            self.assertEqual(
                (root / "xcresult-failure-ax.txt").read_text(), "Application tree\n"
            )
            self.assertTrue((root / "xcresult-failure-screenshot.png").is_file())
            self.assertFalse(list(root.glob(".xcresult-attachment-*")))

    def test_kernel_limit_bounds_single_export_file_during_production(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "payload"
            command = ui_e2e._file_limited_command(
                [
                    sys.executable, "-c",
                    "import pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(b'x'*8192)",
                    str(output),
                ],
                128,
            )
            result = ui_e2e.run_process(
                command, log_path=root / "writer.log",
                timeout_seconds=5, max_output_bytes=4096,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertLessEqual(output.stat().st_size, 128)

    def test_oversize_and_partial_failed_exports_are_removed(self) -> None:
        def capture(command: list[str], **kwargs: object) -> object:
            output = Path(command[command.index("--output-path") + 1])
            output.write_bytes(b"partial" * 10)
            kwargs["log_path"].write_text("fixture export failed", encoding="utf-8")
            payload_id = command[command.index("--id") + 1]
            return ui_e2e.CommandResult(0 if "ax-fixture" in payload_id else 70, 1)

        messages: list[str] = []
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            limits = {
                "xcresult-failure-ax.txt": 8,
                "xcresult-failure-screenshot.png": 128,
            }
            with (
                mock.patch.object(ui_e2e, "run_process", side_effect=capture),
                mock.patch.object(ui_e2e, "FAILURE_ATTACHMENT_LIMITS", limits),
            ):
                ui_e2e.export_failure_attachments(
                    self.activities(), result_bundle=root / "test.xcresult", logs_dir=root,
                    deadline=ui_e2e.time.monotonic() + 10, log=messages.append,
                )
            receipt = json.loads((root / "xcresult-attachment-export.json").read_text())
            self.assertEqual(
                [item["status"] for item in receipt["attachments"]],
                ["unavailable", "unavailable"],
            )
            self.assertIn("byte cap", receipt["attachments"][0]["error"])
            self.assertIn("exited 70", receipt["attachments"][1]["error"])
            self.assertFalse((root / "xcresult-failure-ax.txt").exists())
            self.assertFalse((root / "xcresult-failure-screenshot.png").exists())
            self.assertFalse(list(root.glob(".xcresult-attachment-*")))

    def test_shared_deadline_does_not_start_additional_exports(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(ui_e2e, "run_process") as run:
                ui_e2e.export_failure_attachments(
                    self.activities(), result_bundle=root / "test.xcresult", logs_dir=root,
                    deadline=ui_e2e.time.monotonic() - 1, log=lambda _message: None,
                )
            run.assert_not_called()
            receipt = json.loads((root / "xcresult-attachment-export.json").read_text())
            self.assertTrue(all(
                "deadline exhausted" in item["error"]
                for item in receipt["attachments"]
            ))

    def test_partial_export_never_replaces_original_xcode_failure(self) -> None:
        payloads = FailedXcodeDiagnosticsTests()._payloads()
        payloads["activities"] = self.activities()
        second_case = {
            "nodeType": "Test Case", "name": "testOther",
            "result": "Failed", "nodeIdentifier": "other",
        }
        payloads["tests"]["testNodes"][0]["children"].append(second_case)
        test_ids: list[str] = []

        def capture(command: list[str], **kwargs: object) -> object:
            if "--output-path" in command:
                output = Path(command[command.index("--output-path") + 1])
                output.write_text("Application tree\n", encoding="utf-8")
                kwargs["log_path"].write_text("fixture diagnostic", encoding="utf-8")
                payload_id = command[command.index("--id") + 1]
                return ui_e2e.CommandResult(0 if "ax-fixture" in payload_id else 70, 1)
            if "--test-id" in command:
                test_ids.append(command[command.index("--test-id") + 1])
            kwargs["log_path"].write_text(json.dumps(payloads[command[4]]), encoding="utf-8")
            return ui_e2e.CommandResult(0, 1)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "UIE2E.xcresult"
            bundle.mkdir()
            with mock.patch.object(ui_e2e, "run_process", side_effect=capture):
                with self.assertRaisesRegex(ui_e2e.UIE2EError, "xcodebuild test exited 65"):
                    ui_e2e.finish_xcode_run(
                        ui_e2e.CommandResult(65, 1), result_bundle=bundle, logs_dir=root,
                        expected_target="MacOSNetPlayerUITests", log=lambda _message: None,
                    )
            self.assertEqual(
                test_ids,
                ["UIPlaybackTests/testConnectBrowseAndControlRealPlayback()"] * 2,
            )
            self.assertTrue((root / "xcresult-failure-ax.txt").exists())
            self.assertFalse((root / "xcresult-failure-screenshot.png").exists())
            self.assertFalse((root / "result-validation.json").exists())
            receipt = json.loads((root / "xcresult-attachment-export.json").read_text())
            self.assertEqual(
                [item["status"] for item in receipt["attachments"]],
                ["exported", "unavailable"],
            )

    def test_progress_diagnostics_stage_with_absent_or_partial_result_bundle(self) -> None:
        for partial_bundle in (False, True):
            with self.subTest(partial_bundle=partial_bundle), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                work = root / "work"
                logs = work / "logs"
                logs.mkdir(parents=True)
                progress = logs / "ui-progress.jsonl"
                progress.write_text('{"phase":"xcode_test","event":"started"}\n', encoding="utf-8")
                bundle = work / "result.xcresult"
                if partial_bundle:
                    bundle.mkdir()
                    (bundle / "partial-data").write_bytes(b"partial")
                stager = ui_e2e.ArtifactStager(root / "artifacts")
                ui_e2e._stage_artifacts(stager, work_dir=work, result_bundle=bundle)
                staged = stager.destination / "ui-progress.jsonl"
                self.assertEqual(staged.read_bytes(), progress.read_bytes())
                self.assertLessEqual(staged.stat().st_size, ui_e2e.MAX_PROGRESS_DIAGNOSTIC_BYTES)
                self.assertEqual((stager.destination / "result.xcresult").is_dir(), partial_bundle)
                if not partial_bundle:
                    report = (stager.destination / "artifact-staging.log").read_text()
                    self.assertIn("result.xcresult omitted", report)

    def test_staging_selected_payloads_keeps_allowlist_and_aggregate_caps(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work = root / "work"
            logs = work / "logs"
            logs.mkdir(parents=True)
            (logs / "xcresult-failure-ax.txt").write_bytes(b"a" * 16)
            (logs / "xcresult-failure-screenshot.png").write_bytes(b"x" * 64)
            (logs / "xcresult-attachment-export.json").write_text("{}", encoding="utf-8")
            (logs / "recording.mp4").write_bytes(b"not allowlisted")
            bundle = work / "test.xcresult"
            bundle.mkdir()
            (bundle / "data").write_bytes(b"x" * 128)
            stager = ui_e2e.ArtifactStager(root / "artifacts", max_bytes=256)
            with self.assertRaisesRegex(ui_e2e.UIE2EError, "not allowlisted"):
                stager.stage_file(logs / "recording.mp4")
            with (
                mock.patch.object(ui_e2e, "MAX_RESULT_BUNDLE_BYTES", 64),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                ui_e2e._stage_artifacts(stager, work_dir=work, result_bundle=bundle)
            self.assertTrue((stager.destination / "xcresult-failure-ax.txt").is_file())
            self.assertTrue((stager.destination / "xcresult-failure-screenshot.png").is_file())
            self.assertFalse((stager.destination / "result.xcresult").exists())
            self.assertFalse((stager.destination / "recording.mp4").exists())
            self.assertIn(
                "staged:", (stager.destination / "artifact-staging.log").read_text()
            )
            self.assertLessEqual(
                sum(file.stat().st_size for file in stager.destination.iterdir()), 256
            )
            self.assertLessEqual(stager.used_bytes, 256)
            extra = logs / "xcodebuild.log"
            extra.write_bytes(b"x" * 256)
            with contextlib.redirect_stdout(io.StringIO()):
                stager.stage_file(extra)
            self.assertFalse((stager.destination / "xcodebuild.log").exists())

    def test_staging_oversized_selected_payload_is_explicitly_omitted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "xcresult-failure-screenshot.png"
            source.write_bytes(b"x" * 64)
            stager = ui_e2e.ArtifactStager(root / "artifacts")
            with (
                mock.patch.object(
                    ui_e2e, "FAILURE_ATTACHMENT_LIMITS", {source.name: 32}
                ),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                stager.stage_file(source)
            self.assertEqual(stager.used_bytes, 0)
            self.assertFalse((stager.destination / source.name).exists())
            self.assertIn("omitted", stager.diagnostics[0])


if __name__ == "__main__":
    unittest.main()
