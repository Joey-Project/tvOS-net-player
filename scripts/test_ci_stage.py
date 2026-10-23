from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("ci-stage.py")
SPEC = importlib.util.spec_from_file_location("ci_stage", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
ci_stage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ci_stage)


class IsolatedEnvironmentTests(unittest.TestCase):
    def setUp(self) -> None:
        environment = mock.patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        for key in tuple(os.environ):
            if key.startswith("CI_") or key in ("GITHUB_STEP_SUMMARY", "GITHUB_ENV", "GITHUB_OUTPUT"):
                os.environ.pop(key)


class StageTests(IsolatedEnvironmentTests):
    def setUp(self) -> None:
        super().setUp()
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)

    def run_stage(self, code: str, *, timeout: float = 5, **kwargs: object) -> tuple[int, dict]:
        with contextlib.redirect_stdout(io.StringIO()):
            return ci_stage.run_stage(
                "core-tests", [sys.executable, "-c", code], self.directory,
                timeout_seconds=timeout, **kwargs,
            )

    def test_success_cwd_combined_output_and_null_counts(self) -> None:
        exit_code, result = self.run_stage(
            "import os,sys; print(os.getcwd(),flush=True); print('stderr',file=sys.stderr)",
            cwd=self.directory,
        )
        self.assertEqual(exit_code, 0)
        self.assertEqual(result["actual_exit_code"], 0)
        self.assertEqual(result["cleanup_state"], "verified")
        self.assertIsNone(result["test_summary"])
        log = (self.directory / "core-tests.log").read_text()
        self.assertIn(str(self.directory), log)
        self.assertIn("stderr", log)
        self.assertEqual(json.loads((self.directory / "core-tests.json").read_text()), result)

    def test_nonzero_exit_is_preserved(self) -> None:
        exit_code, result = self.run_stage("raise SystemExit(7)")
        self.assertEqual((exit_code, result["actual_exit_code"]), (7, 7))
        self.assertEqual(result["status"], "failed")

    def test_child_signal_actual_exit_is_not_mistaken_for_success(self) -> None:
        exit_code, result = self.run_stage("import os,signal; os.kill(os.getpid(),signal.SIGTERM)")
        self.assertEqual(exit_code, 128 + signal.SIGTERM)
        self.assertEqual(result["actual_exit_code"], -signal.SIGTERM)
        self.assertFalse(result["timed_out"])

    def test_silent_process_timeout_is_bounded_and_reaped(self) -> None:
        started = time.monotonic()
        exit_code, result = self.run_stage("import time; time.sleep(30)", timeout=0.2)
        self.assertEqual(exit_code, 124)
        self.assertTrue(result["timed_out"])
        self.assertEqual(result["cleanup_state"], "verified")
        self.assertIsNotNone(result["actual_exit_code"])
        self.assertLess(time.monotonic() - started, 5)

    def test_large_output_is_drained_and_does_not_fail_stage(self) -> None:
        count = 200
        exit_code, result = self.run_stage(
            "import os; os.write(1,b'HEAD\\n'); "
            f"[os.write(1,b'x'*65536) for _ in range({count})]; os.write(2,b'\\nTAIL\\n')"
        )
        data = (self.directory / "core-tests.log").read_bytes()
        self.assertEqual(exit_code, 0)
        self.assertTrue(result["log_truncated"])
        self.assertEqual(result["observed_bytes"], count * 65536 + 11)
        self.assertEqual(len(data), ci_stage.MAX_LOG_BYTES)
        self.assertEqual(result["retained_bytes"], len(data))
        self.assertTrue(data.startswith(b"HEAD\n"))
        self.assertTrue(data.endswith(b"\nTAIL\n"))

    def test_timeout_retains_partial_output(self) -> None:
        _, result = self.run_stage("import time; print('partial',flush=True); time.sleep(30)", timeout=0.2)
        self.assertTrue(result["timed_out"])
        self.assertEqual((self.directory / "core-tests.log").read_text(), "partial\n")

    def test_spawn_failure_writes_generic_failure_without_command_arguments(self) -> None:
        secret_argument = "private-argument-not-for-diagnostics"
        with contextlib.redirect_stdout(io.StringIO()):
            exit_code, result = ci_stage.run_stage(
                "core-tests", [str(self.directory / "missing"), secret_argument], self.directory
            )
        self.assertEqual(exit_code, 125)
        self.assertIsNone(result["actual_exit_code"])
        self.assertEqual(result["cleanup_state"], "not_started")
        self.assertNotIn(secret_argument, (self.directory / "core-tests.json").read_text())
        self.assertNotIn("command", result)
        self.assertNotIn("env", result)

    def test_expired_budget_does_not_spawn_and_still_writes_failure(self) -> None:
        os.environ["CI_WORK_DEADLINE_MONOTONIC"] = "-1"
        with mock.patch.object(ci_stage.subprocess, "Popen") as spawn:
            exit_code, result = self.run_stage("raise SystemExit(0)")
        spawn.assert_not_called()
        self.assertEqual(exit_code, 125)
        self.assertEqual(result["status"], "failed")
        self.assertTrue((self.directory / "core-tests.json").is_file())

    def test_exited_leader_pipe_descendant_is_cleaned_without_waiting_stage_deadline(self) -> None:
        owned = []
        popen = ci_stage.subprocess.Popen

        def spawn(*args: object, **kwargs: object) -> subprocess.Popen:
            process = popen(*args, **kwargs)
            owned.append(process)
            return process

        with mock.patch.object(ci_stage.subprocess, "Popen", side_effect=spawn):
            exit_code, result = self.run_stage(
                "import subprocess,sys; child=subprocess.Popen([sys.executable,'-c',"
                "'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(30)']); "
                "print(child.pid,flush=True)"
            )
        self.assertEqual(exit_code, 0)
        self.assertEqual(owned[0].wait(timeout=0), 0)
        self.assertFalse(ci_stage._group_alive(owned[0].pid))
        self.assertEqual(result["cleanup_state"], "verified")
        self.assertFalse(result["timed_out"])

    def test_descendant_without_stdout_is_also_cleaned(self) -> None:
        exit_code, result = self.run_stage(
            "import subprocess,sys; subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'],"
            "stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)"
        )
        self.assertEqual(exit_code, 0)
        self.assertEqual(result["cleanup_state"], "verified")

    def test_cleanup_error_is_not_success_even_with_zero_child_exit(self) -> None:
        stop = ci_stage._stop_group

        def stop_then_report_error(process: subprocess.Popen) -> None:
            stop(process)
            raise PermissionError("private-error-context")

        with mock.patch.object(ci_stage, "_stop_group", side_effect=stop_then_report_error):
            exit_code, result = self.run_stage("pass")
        self.assertEqual(result["actual_exit_code"], 0)
        self.assertEqual(exit_code, 125)
        self.assertEqual(result["cleanup_state"], "unverified")
        self.assertNotIn("private-error-context", json.dumps(result))

    def test_parent_sigterm_writes_result_and_reaps_child(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(SCRIPT), "--artifact-dir", str(self.directory),
             "--stage", "core-tests", "--", sys.executable, "-c",
             "import os,signal,time; print('started',flush=True); "
             "os.kill(os.getppid(),signal.SIGTERM); time.sleep(30)"],
            capture_output=True, timeout=10, check=False,
        )
        self.assertEqual(completed.returncode, 128 + signal.SIGTERM)
        payload = json.loads((self.directory / "core-tests.json").read_text())
        self.assertEqual(payload["interrupted_signal"], signal.SIGTERM)
        self.assertEqual(payload["cleanup_state"], "verified")
        self.assertIsNotNone(payload["actual_exit_code"])

    def test_available_termination_handlers_are_installed_and_restored(self) -> None:
        expected = {getattr(signal, name) for name in ("SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT")
                    if hasattr(signal, name)}
        self.assertEqual(set(ci_stage.HANDLED_SIGNALS), expected)
        previous = {signum: signal.getsignal(signum) for signum in expected}
        with mock.patch.object(ci_stage.signal, "signal", wraps=signal.signal) as install:
            self.run_stage("pass")
        for signum in expected:
            self.assertEqual(sum(call.args[0] == signum for call in install.call_args_list), 2)
            self.assertEqual(signal.getsignal(signum), previous[signum])

    def test_real_cli_uses_explicit_temporary_artifact_environment(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(SCRIPT), "--stage", "core-tests", "--", sys.executable, "-c",
             "print('cli-output'); raise SystemExit(9)"],
            env={**os.environ, "CI_STAGE_ARTIFACT_DIR": str(self.directory)},
            capture_output=True, timeout=10, check=False,
        )
        self.assertEqual(completed.returncode, 9)
        self.assertEqual((self.directory / "core-tests.log").read_text(), "cli-output\n")
        self.assertEqual(json.loads((self.directory / "core-tests.json").read_text())["actual_exit_code"], 9)

    def test_stage_collision_and_unknown_stage_do_not_overwrite(self) -> None:
        self.run_stage("print('original')")
        with self.assertRaises(ci_stage.StageError):
            self.run_stage("print('replacement')")
        self.assertEqual((self.directory / "core-tests.log").read_text(), "original\n")
        with self.assertRaises(ci_stage.StageError):
            ci_stage.run_stage("../outside", [sys.executable], self.directory)

    def test_reaped_leader_cannot_authorize_group_signals(self) -> None:
        process = mock.Mock(returncode=0, pid=1234)
        with mock.patch.object(ci_stage.os, "killpg") as kill:
            with self.assertRaises(ci_stage.StageError):
                ci_stage._stop_group(process)
        kill.assert_not_called()


class BudgetAndBufferTests(IsolatedEnvironmentTests):
    def test_budget_defaults_and_earlier_absolute_deadline(self) -> None:
        self.assertEqual(ci_stage.WORK_SECONDS, 45 * 60)
        with mock.patch.object(ci_stage.time, "monotonic", return_value=100):
            self.assertEqual(ci_stage.work_deadline(None, 2700), 2800)
            self.assertEqual(ci_stage.work_deadline("150", 2700), 150)
            self.assertEqual(ci_stage.work_deadline("9999", 30), 130)
            self.assertEqual(ci_stage.work_deadline(None, 9999), 2800)

    def test_invalid_and_expired_budgets_fail_without_echoing_values(self) -> None:
        for value in ("invalid-private-value", "nan", "inf", "-inf", "-1", "100"):
            with self.subTest(value=value), mock.patch.object(ci_stage.time, "monotonic", return_value=100):
                with self.assertRaises(ci_stage.StageError) as caught:
                    ci_stage.work_deadline(value, 30)
                self.assertNotIn(value, str(caught.exception))
        for timeout in (0, -1, float("nan"), float("inf")):
            with self.assertRaises(ci_stage.StageError):
                ci_stage.work_deadline(None, timeout)

    def test_head_tail_preserves_exact_small_output(self) -> None:
        output = ci_stage.HeadTail()
        for chunk in (b"first\n", b"second", b"\nlast"):
            output.feed(chunk)
        self.assertEqual(bytes(output.head + output.tail), b"first\nsecond\nlast")
        self.assertEqual(output.observed, 17)

    def test_head_tail_memory_is_bounded_with_no_line_delimiters(self) -> None:
        output = ci_stage.HeadTail()
        for _ in range(100):
            output.feed(b"x" * 65536)
            self.assertLessEqual(len(output.head) + len(output.tail), ci_stage.MAX_LOG_BYTES)
        self.assertEqual(output.observed, 100 * 65536)


class SummaryTests(IsolatedEnvironmentTests):
    def test_missing_counts_and_missing_stages_remain_unavailable(self) -> None:
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ, {}, clear=True):
            root = Path(directory)
            with contextlib.redirect_stdout(io.StringIO()):
                result = ci_stage.summarize(root)
            self.assertEqual(set(result), {"schema_version", "stages", "cache_restores"})
            self.assertEqual(result["schema_version"], 1)
            self.assertEqual([stage["stage"] for stage in result["stages"]], list(ci_stage.STAGES))
            self.assertEqual(len(result["stages"]), len(ci_stage.STAGES))
            self.assertTrue(all(stage["status"] == "unavailable" for stage in result["stages"]))
            self.assertTrue(all(stage["test_summary"] is None for stage in result["stages"]))
            self.assertEqual(result["cache_restores"], {"rust": "unavailable", "swift": "unavailable"})

    def test_selected_group_reports_only_its_stages_and_missing_selected_stage(self) -> None:
        with tempfile.TemporaryDirectory() as directory, contextlib.redirect_stdout(io.StringIO()):
            root = Path(directory)
            ci_stage.run_stage("core-tests", [sys.executable, "-c", "pass"], root)
            ci_stage.run_stage("toolchain-apple", [sys.executable, "-c", "pass"], root)
            apple = ci_stage.summarize(root, group="apple")
            core = ci_stage.summarize(root, group="core")

        self.assertEqual([stage["stage"] for stage in apple["stages"]], list(ci_stage.APPLE_STAGES))
        self.assertEqual(apple["stages"][0]["status"], "passed")
        self.assertTrue(all(stage["status"] == "unavailable" for stage in apple["stages"][1:]))
        self.assertNotIn("core-tests", [stage["stage"] for stage in apple["stages"]])
        self.assertEqual([stage["stage"] for stage in core["stages"]], ["core-tests"])
        self.assertEqual(core["stages"][0]["status"], "passed")

        with tempfile.TemporaryDirectory() as directory, contextlib.redirect_stdout(io.StringIO()):
            missing = ci_stage.summarize(Path(directory), group="core")
        self.assertEqual([stage["stage"] for stage in missing["stages"]], ["core-tests"])
        self.assertEqual(missing["stages"][0]["status"], "unavailable")
        self.assertIsNone(missing["stages"][0]["test_summary"])

    def test_invalid_summary_group_is_rejected_by_function_and_cli(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaises(ci_stage.StageError):
                ci_stage.summarize(root, group="unknown")
            completed = subprocess.run(
                [sys.executable, str(SCRIPT), "--artifact-dir", str(root), "--summarize",
                 "--summary-group", "unknown"],
                capture_output=True, timeout=5, check=False,
            )
        self.assertEqual(completed.returncode, 2)

    def test_summary_records_real_results_and_cache_state_not_performance(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            step_summary = root / "step-summary.txt"
            with mock.patch.dict(os.environ, {
                "GITHUB_STEP_SUMMARY": str(step_summary),
                "CI_RUST_CACHE_OUTCOME": "success", "CI_RUST_CACHE_HIT": "true",
                "CI_SWIFT_CACHE_OUTCOME": "success", "CI_SWIFT_CACHE_HIT": "",
            }), contextlib.redirect_stdout(io.StringIO()):
                ci_stage.run_stage("core-tests", [sys.executable, "-c", "pass"], root)
                result = ci_stage.summarize(root)
            core = next(stage for stage in result["stages"] if stage["stage"] == "core-tests")
            self.assertEqual(core["status"], "passed")
            self.assertIsNone(core["test_summary"])
            self.assertEqual(result["cache_restores"], {"rust": "hit", "swift": "miss"})
            self.assertLess(step_summary.stat().st_size, 4096)
            self.assertLess((root / "summary.json").stat().st_size, ci_stage.MAX_SUMMARY_BYTES)

    def test_oversized_or_symlink_artifacts_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "core-tests.log"
            log.write_bytes(b"x" * (ci_stage.MAX_LOG_BYTES + 1))
            with self.assertRaises(ci_stage.StageError):
                ci_stage.summarize(root)
            log.unlink()
            log.symlink_to(root / "missing")
            with self.assertRaises(ci_stage.StageError):
                ci_stage.summarize(root)

    def test_fixed_artifact_caps_fit_aggregate_limit(self) -> None:
        self.assertLessEqual(
            len(ci_stage.STAGES) * (ci_stage.MAX_LOG_BYTES + ci_stage.MAX_JSON_BYTES)
            + ci_stage.MAX_SUMMARY_BYTES, ci_stage.MAX_ARTIFACT_BYTES,
        )

    def test_summary_rejects_invented_counts_and_inconsistent_success(self) -> None:
        with tempfile.TemporaryDirectory() as directory, contextlib.redirect_stdout(io.StringIO()):
            root = Path(directory)
            _, result = ci_stage.run_stage("core-tests", [sys.executable, "-c", "pass"], root)
            for changes in ({"test_summary": {"passed": 0}}, {"actual_exit_code": None},
                            {"timed_out": True}, {"duration_seconds": "private-text"},
                            {"observed_bytes": True}, {"extra": "private-text"}):
                with self.subTest(changes=changes):
                    ci_stage._write_json(root / "core-tests.json", {**result, **changes}, ci_stage.MAX_JSON_BYTES)
                    with self.assertRaises(ci_stage.StageError):
                        ci_stage.summarize(root)

    def test_failed_cache_restore_is_unavailable_not_a_miss_or_hit(self) -> None:
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ, {
            "CI_RUST_CACHE_OUTCOME": "failure", "CI_RUST_CACHE_HIT": "true",
            "CI_SWIFT_CACHE_OUTCOME": "skipped", "CI_SWIFT_CACHE_HIT": "false",
        }), contextlib.redirect_stdout(io.StringIO()):
            result = ci_stage.summarize(Path(directory))
        self.assertEqual(result["cache_restores"], {"rust": "unavailable", "swift": "unavailable"})


def workflow_steps(path: Path) -> dict[str, str]:
    body = path.read_text().split("    steps:\n", 1)[1]
    return {block.splitlines()[0].removeprefix("      - name: "): block
            for block in re.split(r"(?=      - name: )", body) if block.strip()}


def workflow_job(path: Path, job_id: str) -> str:
    source = path.read_text()
    match = re.search(rf"(?m)^  {re.escape(job_id)}:\n", source)
    if match is None:
        raise AssertionError(f"Missing workflow job {job_id}.")
    rest = source[match.end():]
    next_job = re.search(r"(?m)^  [A-Za-z0-9_-]+:\n", rest)
    return rest[:next_job.start()] if next_job else rest


def run_fragment(step: str) -> str:
    lines = step.splitlines()
    for index, line in enumerate(lines):
        if line.startswith("        run: "):
            if line == "        run: |":
                return "\n".join(item[10:] for item in lines[index + 1:] if item.startswith("          "))
            return line.removeprefix("        run: ")
    return ""


class WorkflowTests(IsolatedEnvironmentTests):
    paths = tuple(ci_stage.ROOT / ".github" / "workflows" / name for name in ("ci.yml", "ui-e2e.yml"))

    def test_first_deadlines_real_export_and_all_shell_fragments_parse(self) -> None:
        for path, variable in zip(self.paths, ("CI_WORK_DEADLINE_MONOTONIC", "UI_E2E_JOB_WORK_DEADLINE_MONOTONIC")):
            with self.subTest(path=path.name):
                steps = workflow_steps(path)
                first = next(iter(steps.values()))
                self.assertIn("timeout-minutes: 60", path.read_text().split("    steps:", 1)[0])
                for step in steps.values():
                    fragment = re.sub(r"\$\{\{.*?\}\}", "test-value", run_fragment(step))
                    if fragment:
                        subprocess.run(["bash", "-n"], input=fragment, text=True,
                                       capture_output=True, check=True, timeout=5)
                with tempfile.TemporaryDirectory() as directory:
                    env_file = Path(directory) / "env"
                    started = time.monotonic()
                    subprocess.run(["bash", "-e", "-o", "pipefail", "-c", run_fragment(first)],
                                   env={**os.environ, "GITHUB_ENV": str(env_file), "RUNNER_TEMP": directory},
                                   capture_output=True, check=True, timeout=5)
                    exported = dict(line.split("=", 1) for line in env_file.read_text().splitlines())
                    value = exported[variable]
                    if path.name == "ci.yml":
                        self.assertEqual(exported["CI_STAGE_ARTIFACT_DIR"], f"{directory}/ci-stage-artifacts")
                    self.assertGreaterEqual(float(value), started + 45 * 60)
                    self.assertLessEqual(float(value), time.monotonic() + 45 * 60)

    def test_cache_scope_version_paths_and_optional_failure(self) -> None:
        for path in self.paths:
            steps = workflow_steps(path)
            for language, cache_path in (("Rust", "target"), ("Swift", ".build")):
                restore = steps[f"Restore {language} compiled cache"]
                save = steps[f"Save {language} compiled cache"]
                self.assertIn("uses: actions/cache/restore@v5", restore)
                self.assertIn("uses: actions/cache/save@v5", save)
                for block in (restore, save):
                    self.assertIn("timeout-minutes: 2", block)
                    self.assertIn("continue-on-error: true", block)
                    self.assertIn(f"path: {cache_path}\n", block)
                self.assertNotIn("event_name", restore)
                self.assertNotIn("restore-keys:", restore)
                self.assertIn("success() && github.event_name == 'push'", save)
                self.assertIn("github.ref == format('refs/heads/{0}', github.event.repository.default_branch)", save)
                for value in ("runner.os", "runner.arch", "hashFiles("):
                    self.assertIn(value, restore)
                key = next(line for line in restore.splitlines() if "key: " in line)
                namespace = "ci-" if path.name == "ci.yml" else "ui-${{ matrix.platform }}-"
                self.assertIn(f"key: {namespace}{language.lower()}-v1-", key)
                if language == "Rust":
                    for source in ("Cargo.lock", "Cargo.toml", "rust-toolchain.toml", "build.rs", "Protos/**"):
                        self.assertIn(source, key)
                    self.assertNotIn("Package.resolved", key)
                else:
                    for source in ("Package.swift", "Package.resolved", "Vendor/**", "Sources/**", "Tests/**"):
                        self.assertIn(source, key)
                    self.assertNotIn("Cargo.lock", key)
                    if path.name == "ci.yml":
                        self.assertEqual(
                            re.findall(r"'([^']+)'", key),
                            ["Package.swift", "Package.resolved", "Vendor/**", "Sources/**", "Tests/**",
                             "scripts/test.sh", "scripts/ui-e2e.py"],
                        )

    def test_original_commands_order_concurrency_and_policies_are_preserved(self) -> None:
        ci = self.paths[0].read_text()
        originals = (
            ("pre-commit", "scripts/pre-commit.sh"), ("tvos-build", "scripts/build.sh"),
            ("macos-build", "scripts/build-macos.sh"), ("rust-build", "scripts/build-cache-server.sh"),
            ("tvos-test-build", "scripts/build-for-testing.sh"), ("tvos-tests", "scripts/test-tvos-simulator.sh"),
            ("macos-tests", "scripts/test-macos.sh"),
            ("lan-probe", "bash scripts/test-lan-playback.sh"), ("rust-tests", "scripts/test-cache-server.sh"),
        )
        offsets = []
        for stage, command in originals:
            offsets.append(ci.index(f"run: python3 scripts/ci-stage.py --stage {stage} -- {command}\n"))
        self.assertEqual(offsets, sorted(offsets))
        self.assertEqual(ci.count("run: python3 scripts/ci-stage.py --stage core-tests -- scripts/test.sh\n"), 1)
        self.assertIn("--stage helper-regressions -- python3 -m unittest discover -s scripts -p 'test_*.py' -v", ci)
        for path in self.paths:
            text = path.read_text()
            self.assertIn("permissions:\n  contents: read\n", text)
            self.assertIn("  pull_request:\n  push:\n    branches: [master, main]\n  workflow_dispatch:", text)
            self.assertIn("  cancel-in-progress: true", text)
        ui = self.paths[1].read_text()
        self.assertIn("        platform: [macos, tvos]", ui)
        self.assertIn("      fail-fast: false", ui)
        self.assertIn('run: python3 scripts/ui-e2e.py --platform "${{ matrix.platform }}"', ui)
        self.assertIn("        run: scripts/build-cache-server.sh", ui)

    def test_jobs_partition_all_stages_and_aggregate_both_results(self) -> None:
        path = self.paths[0]
        apple = workflow_job(path, "apple-platforms")
        core = workflow_job(path, "core-unit-tests")
        aggregate = workflow_job(path, "tvos-build-and-tests")
        apple_stages = re.findall(r"python3 scripts/ci-stage.py --stage ([a-z-]+)", apple)
        core_stages = re.findall(r"python3 scripts/ci-stage.py --stage ([a-z-]+)", core)

        self.assertEqual(len(apple_stages), 13)
        self.assertEqual(len(core_stages), 1)
        self.assertEqual(set(apple_stages), set(ci_stage.APPLE_STAGES))
        self.assertEqual(core_stages, ["core-tests"])
        self.assertFalse(set(apple_stages) & set(core_stages))
        self.assertEqual(set(apple_stages) | set(core_stages), set(ci_stage.STAGES))
        self.assertNotIn("needs:", apple)
        self.assertNotIn("needs:", core)
        self.assertTrue(core.split("    steps:\n", 1)[1].startswith(
            "      - name: Establish CI work deadline\n"))
        self.assertIn("timeout-minutes: 60", core)

        self.assertIn("name: tvOS build and tests", aggregate)
        self.assertIn("needs: [apple-platforms, core-unit-tests]", aggregate)
        self.assertIn("if: always()", aggregate)
        self.assertIn("runs-on: ubuntu-latest", aggregate)
        self.assertIn("timeout-minutes: 2", aggregate)
        self.assertNotIn("actions/checkout@", aggregate)
        self.assertIn("APPLE_RESULT: ${{ needs.apple-platforms.result }}", aggregate)
        self.assertIn("CORE_RESULT: ${{ needs.core-unit-tests.result }}", aggregate)
        self.assertIn('"$APPLE_RESULT" != "success"', aggregate)
        self.assertIn('"$CORE_RESULT" != "success"', aggregate)

    def test_aggregate_script_accepts_only_both_success_results(self) -> None:
        step = workflow_steps(self.paths[0])["Require both CI jobs to succeed"]
        fragment = run_fragment(step)
        safe_keys = {
            "PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
            "TERM", "CI", "DEVELOPER_DIR", "SDKROOT", "TOOLCHAINS", "SWIFT_EXEC",
            "SWIFT_DRIVER_SWIFT_FRONTEND_EXEC", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
            "CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR",
        }
        safe_env = {key: value for key, value in os.environ.items() if key in safe_keys}
        safe_env.setdefault("PATH", os.defpath)
        results = ("success", "failure", "cancelled", "skipped", "", "unknown")

        for apple_result in results:
            for core_result in results:
                with self.subTest(apple=apple_result, core=core_result):
                    completed = subprocess.run(
                        ["bash", "-e", "-o", "pipefail", "-c", fragment],
                        env={**safe_env, "APPLE_RESULT": apple_result, "CORE_RESULT": core_result},
                        capture_output=True, timeout=2, check=False,
                    )
                    self.assertEqual(completed.returncode == 0,
                                     apple_result == "success" and core_result == "success")

    def test_upload_is_always_and_exact_allowlist_is_under_aggregate_cap(self) -> None:
        steps = workflow_steps(self.paths[0])
        upload = steps["Upload bounded CI diagnostics"]
        core_upload = steps["Upload bounded core diagnostics"]
        summary = steps["Summarize bounded CI diagnostics"]
        core_summary = steps["Summarize bounded core CI diagnostics"]
        self.assertIn("if: always()", upload)
        self.assertIn("if: always()", core_upload)
        self.assertIn("if: always()", summary)
        self.assertIn("if: always()", core_summary)
        paths = re.findall(r"\$\{\{ env.CI_STAGE_ARTIFACT_DIR \}\}/([^\n]+)", upload)
        expected = {f"{stage}.{suffix}" for stage in ci_stage.APPLE_STAGES for suffix in ("log", "json")}
        self.assertEqual(set(paths), expected | {"summary.json"})
        self.assertEqual(len(paths), len(expected) + 1)
        core_paths = re.findall(r"\$\{\{ env.CI_STAGE_ARTIFACT_DIR \}\}/([^\n]+)", core_upload)
        self.assertEqual(set(core_paths), {"core-tests.log", "core-tests.json", "summary.json"})
        self.assertIn("--summary-group apple", summary)
        self.assertIn("--summary-group core", core_summary)


if __name__ == "__main__":
    unittest.main()
