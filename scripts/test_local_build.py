from __future__ import annotations

import datetime as dt
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("local_build.py")
SPEC = importlib.util.spec_from_file_location("local_build", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
local_build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(local_build)

SYNTHETIC_CDHASH = "B" * 40
CODESIGN_VERBOSE_2_FIXTURE = """Executable=/synthetic/probe
Identifier=dev.joey.tvos-net-player.playback-probe
Format=Mach-O thin (arm64)
CodeDirectory v=20400 size=596 flags=0x0(none) hashes=15+2 location=embedded
Hash type=sha256 size=32
Authority=Apple Development: Synthetic (TESTTEAM01)
TeamIdentifier=TESTTEAM01
"""
CODESIGN_VERBOSE_4_FIXTURE = """Executable=/synthetic/probe
Identifier=dev.joey.tvos-net-player.playback-probe
Format=Mach-O thin (arm64)
CodeDirectory v=20400 size=596 flags=0x0(none) hashes=15+2 location=embedded
Hash type=sha256 size=32
CandidateCDHash sha256=BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
CandidateCDHashFull sha256=BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
Hash choices=sha256
CDHash=BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
Authority=Apple Development: Synthetic (TESTTEAM01)
TeamIdentifier=TESTTEAM01
"""


class LocalBuildTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.config = {
            "schema_version": 1,
            "team_id": "TESTTEAM01",
            "identity_sha1": "A" * 40,
            "certificate_sha1": "A" * 40,
            "expires_at": (dt.date.today() + dt.timedelta(days=30)).isoformat(),
            "durable_root": str(self.root / "LocalBuild"),
        }
        self.config_path = self.root / "signing.json"
        self.write_config()

    def write_config(self) -> None:
        self.config_path.write_text(json.dumps(self.config))

    def test_native_navigation_receipt_isolated_from_legacy_receipt(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        configured_root = self.root / "LocalBuild"
        diagnostic_root = configured_root / local_build.DIAGNOSTIC_ROOT
        source = {"repository": "/repo", "head": "abc", "dirty": False}
        legacy_product = {
            "path": str(configured_root / local_build.PRODUCTS["probe"]),
            "source": source,
            "cdhash": "legacy-cdhash",
        }
        legacy_bytes = (json.dumps({
            "schema_version": 1,
            "products": {"probe": legacy_product},
        }, indent=2, sort_keys=True) + "\n").encode()
        legacy_receipt_path = configured_root / "receipt.json"
        legacy_receipt_path.parent.mkdir(parents=True)
        legacy_receipt_path.write_bytes(legacy_bytes)

        stable_path = diagnostic_root / local_build.DIAGNOSTIC_PRODUCTS[target]
        built_product = {
            "path": str(stable_path),
            "identifier": local_build.DIAGNOSTIC_IDENTIFIERS[target],
            "cdhash": "native-cdhash",
            "source": source,
        }

        def build(received_target, config, root, old_receipt, *, product_path):
            self.assertEqual(received_target, target)
            self.assertEqual(root, diagnostic_root)
            self.assertEqual(product_path, local_build.DIAGNOSTIC_PRODUCTS[target])
            self.assertEqual(old_receipt, {"schema_version": 1, "products": {}})
            return built_product

        with mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_build", side_effect=build), \
                mock.patch.object(local_build, "_source_binding", return_value=source):
            receipt = local_build.prepare_diagnostic_targets({target}, config_file=self.config_path)

        self.assertEqual(receipt["products"], {target: built_product})
        self.assertEqual(receipt["receipt_path"], str(diagnostic_root / "receipt.json"))
        self.assertTrue((diagnostic_root / "receipt.json").is_file())
        self.assertEqual(legacy_receipt_path.read_bytes(), legacy_bytes)
        self.assertFalse((configured_root / ".build-session.lock").exists())
        self.assertFalse((diagnostic_root / ".build-session.lock").exists())

    def test_legacy_receipt_still_rejects_diagnostic_product_key(self) -> None:
        root = self.root / "LocalBuild"
        product = {
            "path": str(root / local_build.DIAGNOSTIC_ROOT / local_build.DIAGNOSTIC_PRODUCTS[local_build.NATIVE_NAVIGATION_TARGET]),
            "source": {"repository": "/repo", "head": "abc", "dirty": False},
            "cdhash": "native-cdhash",
        }
        root.mkdir(parents=True)
        (root / "receipt.json").write_text(json.dumps({
            "schema_version": 1,
            "products": {local_build.NATIVE_NAVIGATION_TARGET: product},
        }))
        with self.assertRaisesRegex(local_build.BuildError, "invalid product entry"):
            local_build._read_receipt(root)

    def test_legacy_status_ignores_existing_diagnostic_product(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        configured_root = self.root / "LocalBuild"
        diagnostic_path = configured_root / local_build.DIAGNOSTIC_ROOT / local_build.DIAGNOSTIC_PRODUCTS[target]
        diagnostic_path.parent.mkdir(parents=True)
        diagnostic_path.write_text("synthetic signed executable placeholder")
        source = {"repository": "/repo", "head": "abc", "dirty": False}
        legacy_product = {
            "path": str(configured_root / local_build.PRODUCTS["probe"]),
            "source": source,
            "cdhash": "legacy-cdhash",
        }
        legacy_receipt = {
            "schema_version": 1,
            "products": {"probe": legacy_product},
        }
        legacy_receipt_path = configured_root / "receipt.json"
        legacy_receipt_path.write_text(json.dumps(legacy_receipt))
        legacy_bytes = legacy_receipt_path.read_bytes()

        with mock.patch.object(local_build, "load_config", return_value={"durable_root": configured_root}), \
                mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_source_binding", return_value=source), \
                mock.patch.object(local_build, "verify_product", return_value={"cdhash": "legacy-cdhash"}):
            result = local_build.status()

        self.assertEqual(set(result["products"]), {"probe"})
        self.assertEqual(legacy_receipt_path.read_bytes(), legacy_bytes)
        self.assertTrue(diagnostic_path.is_file())

    def test_native_navigation_build_compiles_and_signs_to_isolated_path(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        root = self.root / "LocalBuild" / local_build.DIAGNOSTIC_ROOT
        stable_path = root / local_build.DIAGNOSTIC_PRODUCTS[target]
        commands: list[list[str]] = []

        def run(args, **kwargs):
            commands.append(args)
            if args[0] == "xcrun":
                output = Path(args[args.index("-o") + 1])
                output.write_bytes(b"synthetic executable")
            return ""

        with mock.patch.object(local_build, "_run", side_effect=run), \
                mock.patch.object(local_build, "verify_product", return_value={"path": "/staging", "cdhash": "native-cdhash"}), \
                mock.patch.object(local_build, "_source_binding", return_value={"repository": "/repo", "head": "abc", "dirty": False}):
            details = local_build._build(
                target,
                self.config,
                root,
                {"products": {}},
                product_path=local_build.DIAGNOSTIC_PRODUCTS[target],
            )

        compile_command, executable_signing, bundle_signing = commands
        self.assertEqual(compile_command[:4], ["xcrun", "swiftc", "-swift-version", "6"])
        self.assertIn("-strict-concurrency=complete", compile_command)
        self.assertIn("-warnings-as-errors", compile_command)
        self.assertIn(str(SCRIPT.with_name("native-navigation-probe.swift")), compile_command)
        self.assertEqual(executable_signing[0:2], ["codesign", "--force"])
        self.assertEqual(bundle_signing[0:2], ["codesign", "--force"])
        self.assertEqual(executable_signing[executable_signing.index("--sign") + 1], self.config["identity_sha1"])
        self.assertEqual(bundle_signing[bundle_signing.index("--sign") + 1], self.config["identity_sha1"])
        self.assertEqual(executable_signing[executable_signing.index("--identifier") + 1], local_build.DIAGNOSTIC_IDENTIFIERS[target])
        self.assertEqual(bundle_signing[bundle_signing.index("--identifier") + 1], local_build.DIAGNOSTIC_IDENTIFIERS[target])
        self.assertEqual(Path(executable_signing[-1]).name, "native-navigation-probe")
        self.assertEqual(Path(executable_signing[-1]).parent.name, "MacOS")
        self.assertEqual(Path(bundle_signing[-1]).name, "NativeNavigationProbe.app")
        self.assertEqual(details["path"], str(stable_path))
        self.assertTrue(stable_path.is_dir())
        self.assertTrue((stable_path / "Contents/MacOS/native-navigation-probe").is_file())
        with (stable_path / "Contents/Info.plist").open("rb") as stream:
            info = local_build.plistlib.load(stream)
        self.assertEqual(info["CFBundleIdentifier"], local_build.DIAGNOSTIC_IDENTIFIERS[target])
        self.assertEqual(info["CFBundleExecutable"], "native-navigation-probe")
        self.assertEqual(info["CFBundleDisplayName"], "Native Navigation Probe")

    def test_native_navigation_migrates_only_receipted_legacy_path_when_app_is_absent(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        diagnostic_root = self.root / "LocalBuild" / local_build.DIAGNOSTIC_ROOT
        legacy_path = diagnostic_root / local_build.LEGACY_NATIVE_NAVIGATION_PRODUCT
        legacy_path.parent.mkdir(parents=True)
        legacy_path.write_bytes(b"known legacy product")
        source = {"repository": "/repo", "head": "abc", "dirty": False}
        legacy_product = {
            "path": str(legacy_path), "identifier": local_build.DIAGNOSTIC_IDENTIFIERS[target],
            "team_id": "TESTTEAM01", "certificate_sha1": "A" * 40,
            "designated_requirement": 'identifier "native-navigation"', "cdhash": "old-cdhash",
            "source": source,
        }
        receipt_path = diagnostic_root / "receipt.json"
        receipt_path.write_text(json.dumps({"schema_version": 1, "products": {target: legacy_product}}))
        app_path = diagnostic_root / local_build.DIAGNOSTIC_PRODUCTS[target]
        built_product = {
            "path": str(app_path), "identifier": local_build.DIAGNOSTIC_IDENTIFIERS[target],
            "cdhash": "new-cdhash", "executable_cdhash": "new-cdhash", "source": source,
        }

        def build(received_target, config, root, old_receipt, *, product_path):
            self.assertEqual(received_target, target)
            self.assertEqual(root, diagnostic_root)
            self.assertEqual(product_path, local_build.DIAGNOSTIC_PRODUCTS[target])
            self.assertEqual(old_receipt["products"][target], legacy_product)
            self.assertFalse(app_path.exists())
            return built_product

        with mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_build", side_effect=build), \
                mock.patch.object(local_build, "_source_binding", return_value=source):
            with local_build.prepared_diagnostic_targets({target}, config_file=self.config_path) as receipt:
                self.assertEqual(receipt["products"][target], built_product)
        self.assertEqual(legacy_path.read_bytes(), b"known legacy product")
        self.assertEqual(json.loads(receipt_path.read_text())["products"][target]["path"], str(app_path))

    def test_legacy_probe_rebuilds_when_receipt_product_is_missing(self) -> None:
        target = "probe"
        root = self.root / "LocalBuild"
        destination = root / local_build.PRODUCTS[target]
        previous = {"path": str(destination), "designated_requirement": 'identifier "playback-probe"'}

        def run(args, **kwargs):
            if args[0] == "xcrun":
                Path(args[args.index("-o") + 1]).write_bytes(b"rebuilt probe")
            return ""

        with mock.patch.object(local_build, "_run", side_effect=run), \
                mock.patch.object(local_build, "verify_product", return_value={"path": "/staging", "cdhash": "new-cdhash"}), \
                mock.patch.object(local_build, "_source_binding", return_value={"repository": "/repo", "head": "abc", "dirty": False}):
            details = local_build._build(target, self.config, root, {"products": {target: previous}})

        self.assertEqual(details["path"], str(destination))
        self.assertEqual(destination.read_bytes(), b"rebuilt probe")

    def test_native_navigation_rebuilds_when_receipt_binds_missing_current_bundle(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        root = self.root / "LocalBuild" / local_build.DIAGNOSTIC_ROOT
        destination = root / local_build.DIAGNOSTIC_PRODUCTS[target]
        previous = {"path": str(destination), "designated_requirement": 'identifier "native-navigation"'}

        def run(args, **kwargs):
            if args[0] == "xcrun":
                Path(args[args.index("-o") + 1]).write_bytes(b"rebuilt executable")
            return ""

        with mock.patch.object(local_build, "_run", side_effect=run), \
                mock.patch.object(local_build, "verify_product", return_value={"path": "/staging", "cdhash": "new-cdhash", "executable_cdhash": "new-cdhash"}), \
                mock.patch.object(local_build, "_source_binding", return_value={"repository": "/repo", "head": "abc", "dirty": False}):
            details = local_build._build(
                target,
                self.config,
                root,
                {"products": {target: previous}},
                product_path=local_build.DIAGNOSTIC_PRODUCTS[target],
            )

        self.assertEqual(details["path"], str(destination))
        self.assertEqual((destination / "Contents/MacOS/native-navigation-probe").read_bytes(), b"rebuilt executable")

    def test_native_navigation_build_verifies_legacy_receipt_before_migration(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        root = self.root / "LocalBuild" / local_build.DIAGNOSTIC_ROOT
        legacy_path = root / local_build.LEGACY_NATIVE_NAVIGATION_PRODUCT
        legacy_path.parent.mkdir(parents=True)
        legacy_path.write_bytes(b"known legacy product")
        source = {"repository": "/repo", "head": "abc", "dirty": False}
        previous = {
            "path": str(legacy_path), "identifier": local_build.DIAGNOSTIC_IDENTIFIERS[target],
            "team_id": "TESTTEAM01", "certificate_sha1": "A" * 40,
            "designated_requirement": 'identifier "native-navigation"', "cdhash": "old-cdhash",
            "source": source,
        }
        app_path = root / local_build.DIAGNOSTIC_PRODUCTS[target]

        def run(args, **kwargs):
            if args[0] == "xcrun":
                Path(args[args.index("-o") + 1]).write_bytes(b"synthetic executable")
            return ""

        with mock.patch.object(local_build, "_verify_legacy_native_navigation_product") as verify_legacy, \
                mock.patch.object(local_build, "_run", side_effect=run), \
                mock.patch.object(local_build, "verify_product", return_value={"path": "/staging", "cdhash": "new-cdhash", "executable_cdhash": "new-cdhash"}), \
                mock.patch.object(local_build, "_source_binding", return_value=source):
            details = local_build._build(
                target, self.config, root, {"products": {target: previous}},
                product_path=local_build.DIAGNOSTIC_PRODUCTS[target],
            )
        verify_legacy.assert_called_once_with(legacy_path, self.config, previous)
        self.assertEqual(details["path"], str(app_path))
        self.assertTrue(app_path.is_dir())
        self.assertEqual(legacy_path.read_bytes(), b"known legacy product")

    def test_native_navigation_refuses_existing_app_when_receipt_still_binds_legacy_path(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        root = self.root / "LocalBuild" / local_build.DIAGNOSTIC_ROOT
        destination = root / local_build.DIAGNOSTIC_PRODUCTS[target]
        destination.mkdir(parents=True)
        previous = {"path": str(root / local_build.LEGACY_NATIVE_NAVIGATION_PRODUCT), "cdhash": "old"}
        with self.assertRaisesRegex(local_build.BuildError, "not bound to this stable path"):
            local_build._build(target, self.config, root, {"products": {target: previous}}, product_path=local_build.DIAGNOSTIC_PRODUCTS[target])

    def test_native_navigation_refuses_unknown_receipt_path(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        root = self.root / "LocalBuild" / local_build.DIAGNOSTIC_ROOT
        previous = {"path": str(root / "unknown/native-navigation-probe"), "cdhash": "old"}
        with self.assertRaisesRegex(local_build.BuildError, "unknown product path"):
            local_build._build(target, self.config, root, {"products": {target: previous}}, product_path=local_build.DIAGNOSTIC_PRODUCTS[target])

    def test_native_navigation_product_verifies_bundle_and_executable_signatures(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        app = self.root / "NativeNavigationProbe.app"
        executable = app / "Contents/MacOS/native-navigation-probe"
        executable.parent.mkdir(parents=True)
        executable.write_bytes(b"synthetic executable")
        with (app / "Contents/Info.plist").open("wb") as stream:
            local_build.plistlib.dump({
                "CFBundleIdentifier": local_build.DIAGNOSTIC_IDENTIFIERS[target],
                "CFBundleExecutable": "native-navigation-probe",
                "CFBundlePackageType": "APPL",
                "CFBundleName": "Native Navigation Probe",
                "CFBundleDisplayName": "Native Navigation Probe",
            }, stream)
        signature = (
            local_build.DIAGNOSTIC_IDENTIFIERS[target], "TESTTEAM01", "A" * 40,
            'identifier "native-navigation"', SYNTHETIC_CDHASH,
        )
        with mock.patch.object(local_build, "_codesign_details", return_value=signature) as signed_details:
            details = local_build.verify_product(target, app, self.config)
        self.assertEqual(signed_details.call_count, 2)
        self.assertEqual(details["cdhash"], SYNTHETIC_CDHASH)
        self.assertEqual(details["executable_cdhash"], SYNTHETIC_CDHASH)

    def test_native_navigation_print_path_is_the_app_bundle(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        product_path = "/stable/Diagnostics/NativeNavigation/NativeNavigationProbe.app"
        output = io.StringIO()
        with contextlib.redirect_stdout(output), \
                mock.patch.object(local_build, "_prepare_selection", return_value={
                    "products": {target: {"path": product_path}},
                }):
            result = local_build.main(["prepare", target, "--print-path", target])
        self.assertEqual(result, 0)
        self.assertEqual(output.getvalue(), f"{product_path}\n")

    def test_build_rejects_mixing_native_diagnostic_with_legacy_products(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), \
                mock.patch.object(local_build, "prepare_targets") as prepare_legacy, \
                mock.patch.object(local_build, "prepare_diagnostic_targets") as prepare_diagnostic:
            result = local_build.main(["build", "probe", local_build.NATIVE_NAVIGATION_TARGET])
        self.assertEqual(result, 1)
        self.assertIn("must be built separately", stderr.getvalue())
        prepare_legacy.assert_not_called()
        prepare_diagnostic.assert_not_called()

    def test_native_navigation_status_has_a_separate_command(self) -> None:
        with mock.patch.object(local_build, "native_navigation_status", return_value={"products": {}}) as diagnostic_status, \
                mock.patch.object(local_build, "status") as legacy_status:
            result = local_build.main(["native-navigation-status"])
        self.assertEqual(result, 0)
        diagnostic_status.assert_called_once_with()
        legacy_status.assert_not_called()

    def test_run_accepts_native_navigation_target_and_uses_diagnostic_lock_scope(self) -> None:
        target = local_build.NATIVE_NAVIGATION_TARGET
        context = mock.MagicMock()
        context.__enter__.return_value = {
            "products": {target: {"path": "/stable/Diagnostics/NativeNavigation/NativeNavigationProbe.app"}},
        }
        with mock.patch.object(local_build, "prepared_diagnostic_targets", return_value=context) as prepare, \
                mock.patch.object(local_build, "_native_navigation_executable", return_value=Path("/stable/Diagnostics/NativeNavigation/NativeNavigationProbe.app/Contents/MacOS/native-navigation-probe")) as executable_path, \
                mock.patch.object(local_build.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
            result = local_build.main(["run", target, "--", "--self-test"])
        self.assertEqual(result, 0)
        prepare.assert_called_once_with({target})
        executable_path.assert_called_once_with(Path("/stable/Diagnostics/NativeNavigation/NativeNavigationProbe.app"))
        run.assert_called_once_with([
            "/stable/Diagnostics/NativeNavigation/NativeNavigationProbe.app/Contents/MacOS/native-navigation-probe", "--self-test",
        ], check=False)

    def run_server_wrapper(self, system: str, *, ci: str = "", mode: str = "") -> tuple[str, list[str]]:
        bin_dir = self.root / "bin"
        bin_dir.mkdir(exist_ok=True)
        call_log = self.root / "calls"
        for name, body in {
            "uname": 'printf "%s\\n" "$TEST_UNAME"\n',
            "python3": 'printf "%s\\n" python3 "$@" > "$CALL_LOG"\n',
            "cargo": 'printf "%s\\n" cargo "$@" > "$CALL_LOG"\n',
        }.items():
            executable = bin_dir / name
            executable.write_text("#!/bin/sh\n" + body)
            executable.chmod(0o755)

        environment = {
            "PATH": f"{bin_dir}:{os.defpath}",
            "TEST_UNAME": system,
            "CALL_LOG": str(call_log),
        }
        if ci:
            environment["CI"] = ci
        if mode:
            environment["LOCAL_BUILD_MODE"] = mode
        wrapper = Path(__file__).with_name("build-cache-server.sh")
        subprocess.run(["bash", str(wrapper)], env=environment, check=True, timeout=5)
        lines = call_log.read_text().splitlines()
        return lines[0], lines[1:]

    def test_server_wrapper_uses_pinned_helper_only_for_local_darwin(self) -> None:
        cases = (
            ("Darwin", "", "", "python3", [str(SCRIPT.resolve()), "build", "server"]),
            ("Linux", "", "", "cargo", [
                "build", "--manifest-path", str(SCRIPT.parent.parent.resolve() / "Cargo.toml"),
                "--package", "tvos-net-player-cache-server", "--release", "--locked",
            ]),
            ("Darwin", "true", "", "cargo", [
                "build", "--manifest-path", str(SCRIPT.parent.parent.resolve() / "Cargo.toml"),
                "--package", "tvos-net-player-cache-server", "--release", "--locked",
            ]),
            ("Darwin", "", "isolated", "cargo", [
                "build", "--manifest-path", str(SCRIPT.parent.parent.resolve() / "Cargo.toml"),
                "--package", "tvos-net-player-cache-server", "--release", "--locked",
            ]),
        )
        for system, ci, mode, expected_command, expected_args in cases:
            with self.subTest(system=system, ci=ci, mode=mode):
                command, args = self.run_server_wrapper(system, ci=ci, mode=mode)
                self.assertEqual((command, args), (expected_command, expected_args))

    def test_config_requires_identity_and_leaf_fingerprint_to_match(self) -> None:
        self.config["certificate_sha1"] = "B" * 40
        self.write_config()
        with self.assertRaisesRegex(local_build.BuildError, "must match"):
            local_build.load_config(self.config_path)

    def test_config_rejects_expired_certificate(self) -> None:
        self.config["expires_at"] = (dt.date.today() - dt.timedelta(days=1)).isoformat()
        self.write_config()
        with self.assertRaisesRegex(local_build.BuildError, "expired"):
            local_build.load_config(self.config_path)

    def test_durable_root_rejects_relative_paths(self) -> None:
        self.config["durable_root"] = "relative-output"
        self.write_config()
        with self.assertRaisesRegex(local_build.BuildError, "must be an absolute path"):
            local_build.load_config(self.config_path)

    def test_identity_selection_uses_fingerprint_not_certificate_common_name(self) -> None:
        listing = f'  1) {"A" * 40} "Apple Development: Developer (NOTTEAM009)"\n'
        with mock.patch.object(local_build, "_run", return_value=listing):
            local_build.verify_identity(self.config)

    def test_missing_or_invalid_identity_fails_closed(self) -> None:
        with mock.patch.object(local_build, "_run", return_value="0 valid identities found"):
            with self.assertRaisesRegex(local_build.BuildError, "fingerprint is missing"):
                local_build.verify_identity(self.config)

    def test_product_rejects_leaf_or_team_mismatch(self) -> None:
        with mock.patch.object(local_build, "_codesign_details", return_value=(
            local_build.IDENTIFIERS["probe"], "TESTTEAM01", "B" * 40, "identifier \"test\"", SYNTHETIC_CDHASH
        )):
            with self.assertRaisesRegex(local_build.BuildError, "fingerprint mismatch"):
                local_build.verify_product("probe", self.root / "probe", self.config)
        with mock.patch.object(local_build, "_codesign_details", return_value=(
            local_build.IDENTIFIERS["probe"], "OTHERTEAM1", "A" * 40, "identifier \"test\"", SYNTHETIC_CDHASH
        )):
            with self.assertRaisesRegex(local_build.BuildError, "identifier/team mismatch"):
                local_build.verify_product("probe", self.root / "probe", self.config)

    def test_designated_requirement_must_remain_stable(self) -> None:
        previous = {"designated_requirement": 'identifier "old"', "cdhash": "old-hash"}
        with mock.patch.object(local_build, "_codesign_details", return_value=(
            local_build.IDENTIFIERS["probe"], "TESTTEAM01", "A" * 40, 'identifier "new"', SYNTHETIC_CDHASH
        )):
            with self.assertRaisesRegex(local_build.BuildError, "designated requirement changed"):
                local_build.verify_product("probe", self.root / "probe", self.config, previous)

    def test_codesign_verbose_four_metadata_supplies_hash_and_designated_requirement(self) -> None:
        path = self.root / "probe"
        fingerprint = ":".join(["AA"] * 20)
        verbose_two_calls: list[list[str]] = []
        self.assertNotIn("CDHash=", CODESIGN_VERBOSE_2_FIXTURE)
        self.assertIn(f"CDHash={SYNTHETIC_CDHASH}", CODESIGN_VERBOSE_4_FIXTURE)

        def run(args, **kwargs):
            if args[:2] == ["codesign", "--verify"]:
                return subprocess.CompletedProcess(args, 0, "", "")
            if args[:3] == ["codesign", "-dv", "--verbose=4"]:
                return subprocess.CompletedProcess(args, 0, "", CODESIGN_VERBOSE_4_FIXTURE)
            if args[:3] == ["codesign", "-d", "--verbose=2"]:
                verbose_two_calls.append(args)
                return subprocess.CompletedProcess(args, 0, "", CODESIGN_VERBOSE_2_FIXTURE)
            if args[:3] == ["codesign", "-d", "-r-"]:
                return subprocess.CompletedProcess(
                    args, 0, 'designated => identifier "dev.joey.test" and anchor apple\n',
                    "Executable=/synthetic/probe\n",
                )
            if args[:2] == ["codesign", "-d"] and args[2].startswith("--extract-certificates="):
                return subprocess.CompletedProcess(args, 0, "", "")
            if args[0] == "/usr/bin/openssl" and "-fingerprint" in args:
                return subprocess.CompletedProcess(args, 0, f"sha1 Fingerprint={fingerprint}\n", "")
            if args[0] == "/usr/bin/openssl":
                return subprocess.CompletedProcess(args, 0, "subject=CN=Test,OU=TESTTEAM01,O=Example\n", "")
            self.fail(f"unexpected command: {args}")

        with mock.patch.object(local_build, "RUN", side_effect=run) as run_mock:
            details = local_build.verify_product("probe", path, self.config)

        self.assertEqual(details, {
            "path": str(path), "identifier": local_build.IDENTIFIERS["probe"],
            "team_id": "TESTTEAM01", "certificate_sha1": "A" * 40,
            "designated_requirement": 'identifier "dev.joey.test" and anchor apple',
            "cdhash": SYNTHETIC_CDHASH,
        })
        commands = [call.args[0] for call in run_mock.call_args_list]
        self.assertIn(["codesign", "-dv", "--verbose=4", str(path)], commands)
        self.assertNotIn(["codesign", "-d", "--verbose=2", str(path)], commands)
        self.assertEqual(verbose_two_calls, [])

    def test_codesign_metadata_nonzero_exit_is_rejected(self) -> None:
        path = self.root / "probe"

        def run(args, **kwargs):
            if args[:2] == ["codesign", "--verify"]:
                return subprocess.CompletedProcess(args, 0, "", "")
            if args[:3] == ["codesign", "-dv", "--verbose=4"]:
                return subprocess.CompletedProcess(args, 0, "", "Identifier=dev.joey.test\nTeamIdentifier=TESTTEAM01\n")
            if args[:3] == ["codesign", "-d", "-r-"]:
                return subprocess.CompletedProcess(args, 1, 'designated => identifier "dev.joey.test"\n', "metadata unavailable\n")
            self.fail(f"unexpected command: {args}")

        with mock.patch.object(local_build, "RUN", side_effect=run):
            with self.assertRaisesRegex(local_build.BuildError, "could not read signed metadata.*metadata unavailable"):
                local_build._codesign_details(path)

    def test_prepare_selection_and_receipt_use_final_stable_path(self) -> None:
        target = "probe"
        final = self.root / "LocalBuild" / local_build.PRODUCTS[target]
        product = {"path": str(final), "identifier": local_build.IDENTIFIERS[target], "cdhash": "cd-1"}
        with mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_build", return_value=product), \
                mock.patch.object(local_build, "_source_binding", return_value={"repository": "/repo", "head": "abc", "dirty": True}):
            receipt = local_build.prepare_targets({target}, config_file=self.config_path)
        self.assertEqual(receipt["products"][target]["path"], str(final))
        self.assertEqual(receipt["source"]["dirty"], True)
        self.assertTrue(Path(receipt["receipt_path"]).is_file())
        self.assertFalse((self.root / "LocalBuild/.build-session.lock").exists())

    def test_partial_target_failure_keeps_receipt_for_successful_publication(self) -> None:
        root = self.root / "LocalBuild"
        source = {"repository": "/repo", "head": "abc", "dirty": True}
        published = {
            "path": str(root / local_build.PRODUCTS["probe"]),
            "identifier": local_build.IDENTIFIERS["probe"], "cdhash": SYNTHETIC_CDHASH,
            "source": source,
        }

        def build(target, config, durable_root, old_receipt):
            if target == "probe":
                self.assertEqual(durable_root, root)
                return published
            raise local_build.BuildError("synthetic second target failure")

        with mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_build", side_effect=build), \
                mock.patch.object(local_build, "_source_binding", return_value=source):
            with self.assertRaisesRegex(local_build.BuildError, "synthetic second target failure"):
                local_build.prepare_targets({"probe", "server"}, config_file=self.config_path)

        saved = local_build._read_receipt(root)
        self.assertEqual(saved["products"], {"probe": published})
        self.assertEqual(saved["source"], source)
        self.assertFalse((root / ".build-session.lock").exists())

        seen_receipts: list[dict] = []

        def retry_build(target, config, durable_root, old_receipt):
            seen_receipts.append(old_receipt)
            return published

        with mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_build", side_effect=retry_build), \
                mock.patch.object(local_build, "_source_binding", return_value=source):
            local_build.prepare_targets({"probe"}, config_file=self.config_path)
        self.assertEqual(seen_receipts[0]["products"]["probe"], published)

    def test_receipt_read_rejects_missing_malformed_unreadable_and_empty(self) -> None:
        root = self.root / "LocalBuild"
        with self.assertRaisesRegex(local_build.BuildError, "receipt is missing"):
            local_build._read_receipt(root)
        path = root / "receipt.json"
        path.parent.mkdir(parents=True)
        path.write_text("{")
        with self.assertRaisesRegex(local_build.BuildError, "malformed"):
            local_build._read_receipt(root)
        path.unlink()
        path.write_text(json.dumps({"schema_version": 1, "products": {}}))
        with self.assertRaisesRegex(local_build.BuildError, "invalid or empty schema"):
            local_build._read_receipt(root)
        with mock.patch.object(Path, "read_text", side_effect=PermissionError("denied")):
            with self.assertRaisesRegex(local_build.BuildError, "could not read.*denied"):
                local_build._read_receipt(root)

    def test_missing_receipt_is_allowed_only_for_fresh_first_build(self) -> None:
        target = "probe"
        final = self.root / "LocalBuild" / local_build.PRODUCTS[target]
        product = {"path": str(final), "identifier": local_build.IDENTIFIERS[target], "cdhash": "cd-1"}
        with mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_build", return_value=product), \
                mock.patch.object(local_build, "_source_binding", return_value={"repository": "/repo", "head": "abc", "dirty": False}):
            receipt = local_build.prepare_targets({target}, config_file=self.config_path)
        self.assertEqual(set(receipt["products"]), {target})

    def test_missing_receipt_with_orphan_product_is_rejected(self) -> None:
        config = dict(self.config)
        root = self.root / "LocalBuild"
        config["durable_root"] = root
        orphan = root / local_build.PRODUCTS["probe"]
        orphan.parent.mkdir(parents=True)
        orphan.write_text("unreceipted")
        with mock.patch.object(local_build, "load_config", return_value=config), \
                mock.patch.object(local_build, "verify_identity"):
            with self.assertRaisesRegex(local_build.BuildError, "receipt is missing for existing stable product"):
                local_build.prepare_targets({"server"})

    def test_status_accepts_a_valid_partial_receipt_when_other_products_are_absent(self) -> None:
        config = dict(self.config)
        config["durable_root"] = self.root / "LocalBuild"
        path = config["durable_root"] / local_build.PRODUCTS["probe"]
        source = {"repository": "/repo", "head": "abc", "dirty": False}
        item = {"path": str(path), "source": source, "cdhash": "same"}
        receipt = {"schema_version": 1, "products": {"probe": item}}
        with mock.patch.object(local_build, "load_config", return_value=config), \
                mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_read_receipt", return_value=receipt), \
                mock.patch.object(local_build, "_source_binding", return_value=source), \
                mock.patch.object(local_build, "verify_product", return_value={"cdhash": "same"}):
            self.assertEqual(local_build.status(), receipt)

    def test_status_rejects_known_stable_product_without_receipt_entry(self) -> None:
        config = dict(self.config)
        config["durable_root"] = self.root / "LocalBuild"
        orphan = config["durable_root"] / local_build.PRODUCTS["server"]
        orphan.parent.mkdir(parents=True)
        orphan.write_text("unreceipted")
        receipt = {"schema_version": 1, "products": {"probe": {
            "path": str(config["durable_root"] / local_build.PRODUCTS["probe"]),
            "source": {"repository": "/repo", "head": "abc", "dirty": False}, "cdhash": "hash",
        }}}
        with mock.patch.object(local_build, "load_config", return_value=config), \
                mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_read_receipt", return_value=receipt):
            with self.assertRaisesRegex(local_build.BuildError, "no receipt entry for server"):
                local_build.status()

    def test_status_rejects_missing_receipt(self) -> None:
        config = dict(self.config, durable_root=self.root / "LocalBuild")
        with mock.patch.object(local_build, "load_config", return_value=config), \
                mock.patch.object(local_build, "verify_identity"):
            with self.assertRaisesRegex(local_build.BuildError, "receipt is missing"):
                local_build.status()

    def test_app_environment_rejects_nondefault_overrides_before_signing(self) -> None:
        overrides = {
            "PROJECT_PATH": str(self.root / "Other.xcodeproj"),
            "MACOS_SCHEME": "OtherScheme",
            "DERIVED_DATA_PATH": str(self.root / "OtherDerivedData"),
        }
        for key, value in overrides.items():
            with self.subTest(key=key), mock.patch.dict(os.environ, {key: value}, clear=True), \
                    mock.patch.object(local_build, "verify_identity") as verify:
                with self.assertRaisesRegex(local_build.BuildError, f"{key} override is unsupported.*LOCAL_BUILD_MODE=isolated"):
                    local_build.prepare_targets({"app"}, config_file=self.config_path)
                verify.assert_not_called()

    def test_app_environment_accepts_defaults_empty_values_and_canonical_paths(self) -> None:
        root = self.root / "LocalBuild"
        app_config = dict(self.config, durable_root=root)
        derived = root / "DerivedData" / hashlib.sha256(str(local_build.ROOT).encode()).hexdigest()[:16]
        env = {
            "PROJECT_PATH": str(local_build.ROOT / "." / "TVOSNetPlayer.xcodeproj"),
            "MACOS_SCHEME": "",
            "DERIVED_DATA_PATH": str(derived),
        }
        product = {"path": str(root / local_build.PRODUCTS["app"]), "identifier": local_build.IDENTIFIERS["app"], "cdhash": "app-cd"}
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch.object(local_build, "load_config", return_value=app_config), \
                mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_build", return_value=product), \
                mock.patch.object(local_build, "_source_binding", return_value={"repository": "/repo", "head": "abc", "dirty": False}):
            receipt = local_build.prepare_targets({"app"})
        self.assertIn("app", receipt["products"])

    def test_prepared_targets_holds_lock_until_context_exit(self) -> None:
        product = {"path": str(self.root / "x"), "identifier": "x", "cdhash": "cd"}
        with mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_build", return_value=product), \
                mock.patch.object(local_build, "_source_binding", return_value={"repository": "/repo", "head": "abc", "dirty": False}):
            with local_build.prepared_targets({"probe"}, config_file=self.config_path):
                self.assertTrue((self.root / "LocalBuild/.build-session.lock").is_dir())
            self.assertFalse((self.root / "LocalBuild/.build-session.lock").exists())

    def test_unreceipted_existing_product_is_not_overwritten(self) -> None:
        root = self.root / "LocalBuild"
        destination = root / local_build.PRODUCTS["probe"]
        destination.parent.mkdir(parents=True)
        destination.write_text("unrelated")
        with self.assertRaisesRegex(local_build.BuildError, "refusing to overwrite"):
            local_build._build("probe", self.config, root, {"products": {}})
        self.assertEqual(destination.read_text(), "unrelated")

    def test_ci_or_explicit_isolated_mode_keeps_unsigned_wrapper_path(self) -> None:
        with mock.patch.dict(os.environ, {"CI": "true"}, clear=True):
            self.assertTrue(local_build.isolated())
        with mock.patch.dict(os.environ, {"LOCAL_BUILD_MODE": "isolated"}, clear=True):
            self.assertTrue(local_build.isolated())
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertFalse(local_build.isolated())

    def test_status_detects_cdhash_drift(self) -> None:
        config = dict(self.config)
        config["durable_root"] = self.root / "LocalBuild"
        path = config["durable_root"] / local_build.PRODUCTS["probe"]
        receipt = {"products": {"probe": {"path": str(path), "source": {"repository": "/repo", "head": "abc", "dirty": False}, "cdhash": "old"}}}
        with mock.patch.object(local_build, "load_config", return_value=config), \
                mock.patch.object(local_build, "verify_identity"), \
                mock.patch.object(local_build, "_read_receipt", return_value=receipt), \
                mock.patch.object(local_build, "_source_binding", return_value=receipt["products"]["probe"]["source"]), \
                mock.patch.object(local_build, "verify_product", return_value={"cdhash": "new"}):
            with self.assertRaisesRegex(local_build.BuildError, "CDHash differs"):
                local_build.status()


if __name__ == "__main__":
    unittest.main()
