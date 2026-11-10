from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
RUNNER = ROOT / "scripts" / "media-network-e2e.py"


def workflow_job(job_id: str) -> str:
    source = WORKFLOW.read_text(encoding="utf-8")
    match = re.search(rf"(?m)^  {re.escape(job_id)}:\n", source)
    if match is None:
        raise AssertionError(f"Missing workflow job {job_id}.")
    rest = source[match.end():]
    next_job = re.search(r"(?m)^  [A-Za-z0-9_-]+:\n", rest)
    return rest[:next_job.start()] if next_job else rest


def workflow_steps(job: str) -> dict[str, str]:
    body = job.split("    steps:\n", 1)[1]
    return {
        block.splitlines()[0].removeprefix("      - name: "): block
        for block in re.split(r"(?=      - name: )", body)
        if block.strip()
    }


def run_fragment(step: str) -> str:
    lines = step.splitlines()
    for index, line in enumerate(lines):
        if line.startswith("        run: "):
            if line == "        run: |":
                return "\n".join(item[10:] for item in lines[index + 1:] if item.startswith("          "))
            return line.removeprefix("        run: ")
    return ""


def action_step(job: str, action: str) -> str:
    matches = [block for block in re.split(r"(?=      - name: )", job.split("    steps:\n", 1)[1])
               if f"uses: {action}" in block]
    if len(matches) != 1:
        raise AssertionError(f"Expected one workflow step using {action}; found {len(matches)}.")
    return matches[0]


def load_runner():
    spec = importlib.util.spec_from_file_location("media_network_workflow_runner", RUNNER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class MediaNetworkWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.job = workflow_job("media-network")
        cls.steps = workflow_steps(cls.job)

    def test_dedicated_job_and_executed_deadline_setup(self) -> None:
        self.assertIn("name: Native media network acceptance", self.job)
        self.assertIn("runs-on: macos-26", self.job)
        self.assertIn("timeout-minutes: 70", self.job)
        self.assertNotRegex(self.job, r"(?m)^    needs:")
        self.assertIn("defaults:\n      run:\n        shell: bash", self.job)
        names = list(self.steps)
        self.assertIn("deadline", names[0].lower())
        self.assertLess(0, names.index("Check out native media repository"))
        fragment = run_fragment(self.steps[names[0]])
        self.assertIn("MEDIA_NETWORK_E2E_JOB_WORK_DEADLINE_MONOTONIC", fragment)
        self.assertIn("MEDIA_NETWORK_E2E_ARTIFACT_DIR", fragment)
        self.assertRegex(fragment, r"MEDIA_NETWORK_E2E_ARTIFACT_DIR=%s/media-network-e2e-artifacts")
        self.assertIn('"$RUNNER_TEMP"', fragment)
        started = time.monotonic()
        with tempfile.TemporaryDirectory() as directory:
            env_file = Path(directory) / "env"
            subprocess.run(
                ["bash", "-e", "-o", "pipefail", "-c", fragment],
                env={**os.environ, "GITHUB_ENV": str(env_file), "RUNNER_TEMP": directory},
                capture_output=True, check=True, timeout=5,
            )
            exported = dict(line.split("=", 1) for line in env_file.read_text().splitlines())
        deadline = float(exported["MEDIA_NETWORK_E2E_JOB_WORK_DEADLINE_MONOTONIC"])
        self.assertEqual(exported["MEDIA_NETWORK_E2E_ARTIFACT_DIR"],
                         f"{directory}/media-network-e2e-artifacts")
        self.assertGreaterEqual(deadline, started + 60 * 60)
        self.assertLessEqual(deadline, time.monotonic() + 60 * 60)

    def test_all_job_shell_fragments_parse_and_tool_preflight_is_bounded(self) -> None:
        for name, step in self.steps.items():
            fragment = re.sub(r"\$\{\{.*?\}\}", "test-value", run_fragment(step))
            if fragment:
                with self.subTest(step=name):
                    subprocess.run(["bash", "-n"], input=fragment, text=True,
                                   capture_output=True, check=True, timeout=5)
        preflight = self.steps["Compute native media cache identity"]
        self.assertIn("timeout-minutes: 5", preflight)
        fragment = run_fragment(preflight)
        self.assertRegex(fragment, r"if ! command -v ffmpeg.*command -v ffprobe")
        self.assertIn("brew install ffmpeg", fragment)
        self.assertIn("head -c 65537", fragment)
        self.assertIn("local LC_ALL=C output", fragment)
        self.assertIn("${#output} -gt 65536", fragment)

    def test_cache_identity_hashes_bounded_apple_and_rust_toolchain_versions(self) -> None:
        fragment = run_fragment(self.steps["Compute native media cache identity"])
        expected = {
            "apple": "xcodebuild -version",
            "swift": "xcrun swift --version",
            "sdks": "xcodebuild -showsdks",
            "clang": "xcrun clang --version",
            "os": "sw_vers -buildVersion",
            "rust": "rustc -Vv",
            "cargo": "cargo -V",
        }
        for variable, command in expected.items():
            with self.subTest(variable=variable):
                self.assertIn(f'{variable}="$(capture_version {command})"', fragment)
        hash_line = next(line for line in fragment.splitlines() if "shasum -a 256" in line)
        for variable in expected:
            self.assertIn(f'"${variable}"', hash_line)
        self.assertIn("Apple toolchain:\\n%s\\nSwift:", fragment)
        self.assertIn('"$apple" "$swift" "$sdks" "$clang" "$os" "$rust" "$cargo"', fragment)
        self.assertIn("printf 'toolchain=%s\\n' \"$toolchain\" >> \"$GITHUB_OUTPUT\"", fragment)

    def test_helper_and_acceptance_commands_preserve_strict_cases_and_mute(self) -> None:
        helper = self.steps["Run native media helper regressions"]
        self.assertIn("timeout-minutes: 3", helper)
        self.assertIn("python3 -m unittest discover -s scripts -p 'test_media_network*.py' -v", helper)
        candidates = [step for step in self.steps.values()
                      if "scripts/media-network-e2e.py" in run_fragment(step)]
        self.assertEqual(len(candidates), 1)
        step = candidates[0]
        self.assertIn("timeout-minutes: 52", step)
        self.assertNotIn("continue-on-error:", step)
        normalized = re.sub(r"\s+", " ", run_fragment(step)).strip()
        self.assertEqual(
            normalized,
            'python3 scripts/media-network-e2e.py --ci-unsigned --artifact-dir "$MEDIA_NETWORK_E2E_ARTIFACT_DIR"',
        )
        self.assertNotIn("osascript", self.job)
        runner = load_runner()
        self.assertEqual(len(runner.EXPECTED_TESTS), 3)
        self.assertEqual(set(runner.EXPECTED_CASES), set(runner.EXPECTED_TESTS))
        self.assertNotIn(runner.EXTENDED_DIAGNOSTIC_TEST, runner.EXPECTED_TESTS)
        self.assertEqual(runner.EXPECTED_ABR_SIZES, ((640, 360), (320, 180), (640, 360)))
        self.assertEqual(runner.EXPECTED_TRANSCODE_SIZES, ((640, 360),))
        self.assertIn("_mute_system_audio(logs, env)", RUNNER.read_text(encoding="utf-8"))
        self.assertIn("GITHUB_ACTIONS", RUNNER.read_text(encoding="utf-8"))

    def test_runner_budget_and_workflow_leave_diagnostics_time(self) -> None:
        runner = load_runner()
        self.assertEqual(runner.TOTAL_BUDGET_SECONDS, 50 * 60)
        self.assertEqual(runner.COMPILE_BUDGET_SECONDS, 25 * 60)
        self.assertEqual(runner.RUNTIME_BUDGET_SECONDS, 24 * 60)
        self.assertEqual(runner.FINALIZE_BUDGET_SECONDS, 60)
        self.assertEqual(runner.COMPILE_BUDGET_SECONDS + runner.RUNTIME_BUDGET_SECONDS
                         + runner.FINALIZE_BUDGET_SECONDS, runner.TOTAL_BUDGET_SECONDS)
        self.assertGreater(52 * 60, runner.TOTAL_BUDGET_SECONDS)
        self.assertGreaterEqual(70 * 60 - 5 * 60 - 3 * 60 - 52 * 60 - 1 * 60 - 3 * 60, 5 * 60)

    def test_media_rust_cache_has_unique_scoped_optional_policy(self) -> None:
        restore = self.steps["Restore native media Rust cache"]
        save = self.steps["Save native media Rust cache"]
        for block, action in ((restore, "actions/cache/restore@v5"), (save, "actions/cache/save@v5")):
            self.assertIn(f"uses: {action}", block)
            self.assertIn("timeout-minutes: 2", block)
            self.assertIn("continue-on-error: true", block)
            self.assertIn("path: target", block)
        self.assertNotIn("restore-keys:", restore)
        self.assertNotIn("event_name", restore)
        self.assertIn("runner.os", restore)
        self.assertIn("runner.arch", restore)
        self.assertIn("hashFiles(", restore)
        key = next(line for line in restore.splitlines() if "key: " in line)
        self.assertIn("key: media-rust-v1-", key)
        self.assertIn("Cargo.lock", key)
        self.assertIn("Cargo.toml", key)
        self.assertNotIn("Package.resolved", key)
        self.assertNotIn("'Sources/TVOSNetPlayer/", key)
        self.assertIn("Sources/TVOSNetPlayerCacheClient/Protos/**", key)
        self.assertNotIn("Tests/", key)
        self.assertIn("success() && github.event_name == 'push'", save)
        self.assertIn("github.ref == format('refs/heads/{0}', github.event.repository.default_branch)", save)

    def test_diagnostics_upload_is_always_bounded_and_short_retention(self) -> None:
        upload = action_step(self.job, "actions/upload-artifact@v4")
        self.assertIn("if: always()", upload)
        self.assertIn("timeout-minutes: 3", upload)
        self.assertIn("MEDIA_NETWORK_E2E_ARTIFACT_DIR", upload)
        self.assertIn("path: ${{ env.MEDIA_NETWORK_E2E_ARTIFACT_DIR }}", upload)
        self.assertIn("if-no-files-found: warn", upload)
        self.assertIn("retention-days: 3", upload)


if __name__ == "__main__":
    unittest.main()
