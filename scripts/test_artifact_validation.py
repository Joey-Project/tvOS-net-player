from __future__ import annotations

import json
import importlib.util
import marshal
import struct
import sys
import tempfile
import unittest
import zipfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

import artifact_validation as validation
import test_verify_ci_artifacts as fixtures
from unittest.mock import patch


class ArtifactValidationContractTests(unittest.TestCase):
    def test_preserved_artifact_and_fixture_contracts(self) -> None:
        self.assertEqual((validation.MAX_ZIP, validation.MAX_RAW_ARCHIVES,
                          validation.MAX_EXPANDED, validation.MAX_DIAGNOSTICS),
                         (16 * 1024 * 1024, 48 * 1024 * 1024,
                          64 * 1024 * 1024, 4 * 1024 * 1024))
        self.assertEqual(len(validation.NATIVE_FILES), 9)
        self.assertEqual(len(validation.UI_JSON_NAMES), 5)
        self.assertEqual(len(fixtures.producer_media.EXPECTED_TESTS), 3)
        ui = fixtures.producer_media.ui
        self.assertEqual(set(ui.EXPECTED_CASES_BY_PLATFORM), {"macos", "tvos"})
        self.assertEqual({key: len(value) for key, value in ui.EXPECTED_CASES_BY_PLATFORM.items()},
                         {"macos": 5, "tvos": 5})
        self.assertEqual(set(ui.EXPECTED_TASK_IDS), {"automatic", "manual", "silent", "resumption"})
        self.assertEqual(validation.REPORT_CONTRACT, "native3-ui5-fixture4")

    def test_synthetic_native_report_corpus_passes_all_three_released_assertions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            files, expected = fixtures._native_report_files(Path(directory))
        self.assertEqual(set(files), validation.NATIVE_FILES)
        self.assertEqual(len(expected["reports"]), 3)
        self.assertEqual({entry["case_full_name"] for entry in expected["reports"]},
                         set(fixtures.producer_media.EXPECTED_TESTS))

    def test_scope_is_closed_and_migration_is_exact(self) -> None:
        harness = fixtures.Harness()
        try:
            validation._scope_identity(harness.scope)
            invalid = dict(harness.scope, skip_helpers=True)
            with self.assertRaisesRegex(validation.VerificationError, "fields/schema mismatch"):
                validation._scope_identity(invalid)
            migration = dict(harness.scope)
            migration["route"] = "helper-migration"
            migration["migration"] = {
                "path": "scripts/not-a-helper.py", "from_sha256": "1" * 64,
                "to_sha256": "2" * 64, "report_contract": validation.REPORT_CONTRACT,
            }
            with self.assertRaisesRegex(validation.VerificationError, "migration path"):
                validation._scope_identity(migration)
        finally:
            harness.close()

    def test_expiry_requires_future_timezone_aware_timestamp(self) -> None:
        now = datetime(2030, 1, 1, tzinfo=timezone.utc)
        valid = (now + timedelta(days=1)).isoformat()
        self.assertEqual(validation._validate_expiry(valid, "test", now), datetime.fromisoformat(valid))
        for value in ("not-a-date", "2030-01-02T00:00:00", "2029-12-31T23:59:59Z"):
            with self.subTest(value=value), self.assertRaises(validation.VerificationError):
                validation._validate_expiry(value, "test", now)

    def test_media_and_transitive_ui_execute_only_the_verified_source_snapshot(self) -> None:
        harness = fixtures.Harness()
        try:
            media_path = harness.control / "media-network-e2e.py"
            ui_path = harness.control / "ui-e2e.py"
            manifest = harness.control_manifest
            control_before = validation._check_control_bundle(
                harness.control, harness.release, harness.control_digest,
            )
            marker = harness.root / "replacement-executed.txt"
            media_pyc = Path(importlib.util.cache_from_source(str(media_path)))
            harness.control.chmod(0o755)
            media_pyc.parent.mkdir(parents=True)
            media_source, _ = validation._read_file(media_path, validation.MAX_CONTROL_FILE, "media")
            ui_source, _ = validation._read_file(ui_path, validation.MAX_CONTROL_FILE, "ui")

            def write_timestamp_pyc(source_path: Path, source: bytes) -> None:
                cache_path = Path(importlib.util.cache_from_source(str(source_path)))
                info = source_path.stat()
                canary = f"from pathlib import Path\nPath({str(marker)!r}).write_text('pyc')\n"
                canary += "#" * (len(source) - len(canary))
                code = compile(canary, str(source_path), "exec")
                cache_path.write_bytes(importlib.util.MAGIC_NUMBER + struct.pack(
                    "<III", 0, int(info.st_mtime), len(source),
                ) + marshal.dumps(code))

            write_timestamp_pyc(media_path, media_source)
            write_timestamp_pyc(ui_path, ui_source)
            for cache_file in media_pyc.parent.iterdir():
                cache_file.chmod(0o444)
            media_pyc.parent.chmod(0o555)
            harness.control.chmod(0o555)

            real_compile = compile
            replaced = False

            def replace_after_verification(source: Any, filename: str, mode: str, *args: Any, **kwargs: Any) -> Any:
                nonlocal replaced
                if not replaced:
                    replaced = True
                    harness.control.chmod(0o755)
                    media_path.chmod(0o644)
                    ui_path.chmod(0o644)
                    media_path.write_text(
                        f"from pathlib import Path\nPath({str(marker)!r}).write_text('media')\n",
                        encoding="utf-8",
                    )
                    ui_path.write_text(
                        f"from pathlib import Path\nPath({str(marker)!r}).write_text('ui')\n",
                        encoding="utf-8",
                    )
                    media_path.chmod(0o444)
                    ui_path.chmod(0o444)
                    harness.control.chmod(0o555)
                return real_compile(source, filename, mode, *args, **kwargs)

            preloaded_ui = sys.modules.pop("media_network_ui_e2e", None)
            try:
                with patch.object(validation, "compile", replace_after_verification):
                    module = validation._load_media_control(harness.control, manifest)
            finally:
                sys.modules.pop("media_network_ui_e2e", None)
                if preloaded_ui is not None:
                    sys.modules["media_network_ui_e2e"] = preloaded_ui
            self.assertTrue(replaced)
            self.assertFalse(marker.exists())
            self.assertEqual(Path(module.ui.__file__), ui_path)
            self.assertEqual(set(module.EXPECTED_TESTS), set(fixtures.producer_media.EXPECTED_TESTS))
            with self.assertRaisesRegex(validation.VerificationError, "control bundle directory layout mismatch"):
                validation._same_control(
                    control_before,
                    validation._check_control_bundle(
                        harness.control, harness.release, harness.control_digest,
                    ),
                )
        finally:
            harness.close()

    def test_zip_traversal_and_expansion_limits_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            traversal = root / "traversal.zip"
            with zipfile.ZipFile(traversal, "w") as archive:
                archive.writestr("../outside.json", b"{}")
            with self.assertRaisesRegex(validation.VerificationError, "traversal|unsafe"):
                validation._inspect_zip(traversal, kind="macos", expansion_cap=1024)
            expanded = root / "expanded.zip"
            with zipfile.ZipFile(expanded, "w") as archive:
                archive.writestr("payload.bin", b"x" * 2048)
            with self.assertRaisesRegex(validation.VerificationError, "expansion cap"):
                validation._inspect_zip(expanded, kind="macos", expansion_cap=1024)

    def test_fixture_contract_rejects_missing_scenario_and_bad_metric(self) -> None:
        ui = fixtures.producer_media.ui
        metrics = fixtures._fixture_metrics()
        valid = ui.validate_task_fixture_metrics(metrics)
        self.assertEqual(set(valid["scenarios"]), {"automatic", "manual", "silent", "resumption"})
        missing = json.loads(json.dumps(metrics))
        del missing["scenarios"]["silent"]
        with self.assertRaises(ui.UIE2EError):
            ui.validate_task_fixture_metrics(missing)
        invalid = json.loads(json.dumps(metrics))
        invalid["scenarios"]["manual"]["create_count"] = 2
        with self.assertRaises(ui.UIE2EError):
            ui.validate_task_fixture_metrics(invalid)


if __name__ == "__main__":
    unittest.main()
