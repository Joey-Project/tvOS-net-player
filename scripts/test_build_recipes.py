from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


class RecipeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        shutil.copy2(ROOT / "scripts/test-ui-e2e.sh", scripts)
        (scripts / "ui-e2e.py").touch()
        self.log = self.root / "calls.log"
        (scripts / "build-cache-server.sh").write_text(
            "#!/usr/bin/env bash\nset -euo pipefail\nprintf 'build\\n' >> \"$CALL_LOG\"\n"
        )
        (scripts / "local_build.py").touch()
        fakebin = self.root / "bin"
        fakebin.mkdir()
        fake_python = fakebin / "python3"
        fake_python.write_text(
            "#!/usr/bin/env bash\nset -euo pipefail\n"
            "printf 'python %s CACHE_SERVER_BINARY=%s PROJECT_PATH=%s MACOS_SCHEME=%s DERIVED_DATA_PATH=%s\\n' \"$*\" \"${CACHE_SERVER_BINARY-}\" \"${PROJECT_PATH-}\" \"${MACOS_SCHEME-}\" \"${DERIVED_DATA_PATH-}\" >> \"$CALL_LOG\"\n"
            "if [[ \"$1\" == */local_build.py ]]; then\n"
            "  case \"${PREPARE_RESULT:-/pinned/server}\" in\n"
            "    fail) exit 23 ;;\n"
            "    empty) exit 0 ;;\n"
            "    *) printf '%s\\n' \"${PREPARE_RESULT:-/pinned/server}\" ;;\n"
            "  esac\n"
            "fi\n"
        )
        fake_python.chmod(0o755)
        (scripts / "build-cache-server.sh").chmod(0o755)
        self.env = os.environ.copy()
        self.env.update(PATH=f"{fakebin}:{self.env['PATH']}", CALL_LOG=str(self.log))
        for key in ("CI", "LOCAL_BUILD_MODE", "CACHE_SERVER_BINARY", "PREPARE_RESULT"):
            self.env.pop(key, None)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_wrapper(self, platform: str, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(self.root / "scripts/test-ui-e2e.sh"), platform],
            env={**self.env, **env}, text=True, capture_output=True, check=False,
        )

    def calls(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_explicit_binary_is_honored_on_both_platforms(self) -> None:
        for platform in ("macos", "tvos"):
            for mode in ({"CI": "true"}, {"LOCAL_BUILD_MODE": "isolated"}):
                with self.subTest(platform=platform, mode=mode):
                    self.log.unlink(missing_ok=True)
                    result = self.run_wrapper(
                        platform, CACHE_SERVER_BINARY="/fixture/server", **mode,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    calls = self.calls()
                    self.assertEqual(len(calls), 1)
                    self.assertIn(f"--platform {platform}", calls[0])
                    self.assertIn("CACHE_SERVER_BINARY=/fixture/server", calls[0])

    def test_pinned_preparation_is_used_only_when_unset(self) -> None:
        result = self.run_wrapper("macos", PREPARE_RESULT="/pinned/server")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.calls()), 2)
        self.assertIn("CACHE_SERVER_BINARY=/pinned/server", self.calls()[-1])

    def test_prepare_failure_does_not_launch_ui_even_with_stale_default(self) -> None:
        stale = self.root / "target/release/tvos-net-player-cache-server"
        stale.parent.mkdir(parents=True)
        stale.write_text("stale binary")
        result = self.run_wrapper("macos", PREPARE_RESULT="fail")
        self.assertEqual(result.returncode, 23)
        self.assertEqual(len(self.calls()), 1)

    def test_empty_prepare_result_is_rejected(self) -> None:
        result = self.run_wrapper("tvos", PREPARE_RESULT="empty")
        self.assertEqual(result.returncode, 1)
        self.assertIn("empty CACHE_SERVER_BINARY", result.stderr)
        self.assertEqual(len(self.calls()), 1)

    def test_isolated_ci_uses_legacy_build_and_release_binary(self) -> None:
        for env in ({"CI": "true"}, {"LOCAL_BUILD_MODE": "isolated"}):
            with self.subTest(env=env):
                self.log.unlink(missing_ok=True)
                result = self.run_wrapper("tvos", **env)
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = self.calls()
                self.assertEqual(len(calls), 2)
                self.assertEqual(calls[0], "build")
                self.assertIn("CACHE_SERVER_BINARY=" + str(self.root / "target/release/tvos-net-player-cache-server"), calls[1])

    def test_invalid_platform_fails_before_any_tool(self) -> None:
        result = self.run_wrapper("ios")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.calls(), [])

    def test_build_macos_delegates_to_pinned_helper_outside_isolated_modes(self) -> None:
        result = self.run_build_macos(
            PROJECT_PATH="/custom/project.xcodeproj", MACOS_SCHEME="CustomScheme",
            DERIVED_DATA_PATH="/custom/derived",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual(len(calls), 1)
        self.assertIn(f"python {ROOT / 'scripts/local_build.py'} build app", calls[0])
        self.assertIn("PROJECT_PATH=/custom/project.xcodeproj", calls[0])
        self.assertIn("MACOS_SCHEME=CustomScheme", calls[0])
        self.assertIn("DERIVED_DATA_PATH=/custom/derived", calls[0])

    def test_build_macos_isolated_xcodebuild_keeps_custom_overrides(self) -> None:
        xcodebuild = self.root / "bin/xcodebuild"
        xcodebuild.write_text(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" > \"$BUILD_LOG\"\n"
        )
        xcodebuild.chmod(0o755)
        env = {
            "LOCAL_BUILD_MODE": "isolated", "PROJECT_PATH": "/custom/project.xcodeproj",
            "MACOS_SCHEME": "CustomScheme", "CONFIGURATION": "Release",
            "MACOS_DESTINATION": "platform=macOS", "DERIVED_DATA_PATH": "/custom/derived",
            "BUILD_LOG": str(self.root / "build.log"),
        }
        for mode in ({"LOCAL_BUILD_MODE": "isolated"}, {"CI": "true"}):
            with self.subTest(mode=mode):
                (self.root / "build.log").unlink(missing_ok=True)
                result = self.run_build_macos(**{**env, **mode})
                self.assertEqual(result.returncode, 0, result.stderr)
                args = (self.root / "build.log").read_text()
                for value in ("/custom/project.xcodeproj", "CustomScheme", "Release", "platform=macOS", "/custom/derived"):
                    self.assertIn(value, args)

    def run_build_macos(self, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(ROOT / "scripts/build-macos.sh")],
            env={**self.env, **env}, text=True, capture_output=True, check=False,
        )


if __name__ == "__main__":
    unittest.main()
