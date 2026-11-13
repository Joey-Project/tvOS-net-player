from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile
from datetime import datetime, timedelta, timezone
from io import BytesIO
from pathlib import Path
from typing import Any

from test_media_network_e2e import media as producer_media, report_payload


SCRIPTS = Path(__file__).resolve().parent
REPOSITORY = "Joey-Project/tvOS-net-player"
REPOSITORY_ID = 1255447318
HELPERS = ("scripts/media-network-e2e.py", "scripts/ui-e2e.py")
MANIFEST_FIELDS = {
    "schema_version", "repository", "repository_id", "pr_number", "base_sha",
    "feature_head_sha", "feature_tree_sha", "runs", "artifacts",
}
NATIVE_MEMBERS = {
    "logs/cargo-compile.log", "logs/cargo-test.log", "logs/swift-compile.log",
    "logs/system-mute-set.log", "logs/system-mute-verify.log", "rust/abr.json",
    "rust/buffered.json", "rust/transcoding.json", "summary.json",
}
UI_BASENAMES = (
    "xcresult-summary.json", "xcresult-tests.json", "result-validation.json",
    "task-fixture-metrics.json", "task-fixture-validation.json",
)


def _json_bytes(value: Any) -> bytes:
    return (json.dumps(value, sort_keys=True, indent=2, ensure_ascii=True) + "\n").encode("utf-8")


def _write_json(path: Path, value: Any) -> None:
    path.write_bytes(_json_bytes(value))


def _digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _fixture_metrics() -> dict[str, Any]:
    ui = producer_media.ui
    return {
        "schema_version": 1,
        "scenarios": {
            name: {
                "task_id": ui.EXPECTED_TASK_IDS[name],
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
                    [ui.EXPECTED_PLAYBACK_SOURCES["resumption"]] * 2 if name == "resumption" else []
                ),
                "rpc_trace": ["watch", "get_task", "get_task"] if name == "silent" else [],
                **({"manual_resume_read_before_watch": True} if name == "manual" else {}),
            }
            for name in ("automatic", "manual", "silent", "resumption")
        },
    }


def _native_report_files(directory: Path) -> tuple[dict[str, bytes], dict[str, Any]]:
    media = producer_media
    reports = directory / "rust"
    reports.mkdir(parents=True)
    for case_name, contract in media.EXPECTED_CASES.items():
        payload = report_payload(
            contract["sizes"], scenario=contract["scenario"], duration=contract["duration"],
            probe_duration=contract["probe_duration"], probe_deadline=contract["probe_deadline"],
        )
        payload["case_full_name"] = case_name
        if case_name == media.EXPECTED_TESTS[1]:
            payload["probe"]["video_size_expectations"]["entries"] = payload["probe"][
                "video_size_expectations"]["entries"][:1]
        if case_name == media.EXPECTED_TESTS[2]:
            payload["requests"]["media_payload_by_phase_and_representation"][2][0] = 0
            payload["probe"]["video_size_expectations"]["entries"] = payload["probe"][
                "video_size_expectations"]["entries"][:2]
            probe = payload["probe"]
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
        _write_json(reports / contract["report"], payload)
    diagnostics = media._validate_reports_and_derive_diagnostics(reports)
    files = {
        "logs/cargo-compile.log": b"compile passed\n",
        "logs/cargo-test.log": (
            "test media_acceptance_tests::native_progressive_adaptation_and_recovery ... ok\n"
            "test media_acceptance_tests::native_transcoded_output_decodes ... ok\n"
            "test media_acceptance_tests::native_progressive_buffered_fallback_keeps_playing ... ok\n"
            "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out\n"
        ).encode(),
        "logs/swift-compile.log": b"compile passed\n",
        "logs/system-mute-set.log": b"true\n",
        "logs/system-mute-verify.log": b"true\n",
        "rust/abr.json": (reports / "abr.json").read_bytes(),
        "rust/buffered.json": (reports / "buffered.json").read_bytes(),
        "rust/transcoding.json": (reports / "transcoding.json").read_bytes(),
        "summary.json": _json_bytes({
            "schema_version": 1, "status": "passed", "recovery_latency_diagnostics": diagnostics,
        }),
    }
    return files, diagnostics


def _ui_archive(platform: str) -> bytes:
    ui = producer_media.ui
    expected = ui.EXPECTED_CASES_BY_PLATFORM[platform]
    target = ui.PLATFORMS[platform].test_target
    tests = {"testNodes": [{
        "nodeType": "UI test bundle", "name": target,
        "children": [{"nodeType": "Test Case", "name": name, "result": "Passed"} for name in expected],
    }]}
    summary = {"totalTestCount": 5, "passedTests": 5, "failedTests": 0, "skippedTests": 0}
    counts = ui.validate_xcresult(summary, tests, target, expected)
    metrics = _fixture_metrics()
    fixture = ui.validate_task_fixture_metrics(metrics)
    payloads = {
        "xcresult-summary.json": summary,
        "xcresult-tests.json": tests,
        "result-validation.json": counts,
        "task-fixture-metrics.json": metrics,
        "task-fixture-validation.json": fixture,
    }
    stream = BytesIO()
    with zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, payload in payloads.items():
            archive.writestr(f"Reports/{name}", _json_bytes(payload))
    return stream.getvalue()


def _native_archive(directory: Path) -> bytes:
    files, _ = _native_report_files(directory)
    stream = BytesIO()
    with zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name in sorted(NATIVE_MEMBERS):
            archive.writestr(name, files[name])
    return stream.getvalue()


class Harness:
    def __init__(self, *, migration: bool = False, canary: Path | None = None) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="artifact-control-tests-")
        self.root = Path(self.temp.name).resolve()
        self.scope_path = self.root / "trusted-scope.json"
        self.checkout = self.root / "subject"
        self.control = self.root / "control"
        self.input = self.root / "raw-input"
        self.output = self.root / "derived-output"
        self.input.mkdir()
        self._make_subject(migration=migration, canary=canary)
        self._make_control()
        self.scope = self._make_evidence()
        _write_json(self.scope_path, self.scope)

    def close(self) -> None:
        if self.control.exists():
            self.control.chmod(0o755)
            for path in self.control.iterdir():
                if path.is_file():
                    path.chmod(0o644)
        self.temp.cleanup()

    def _git(self, *args: str) -> str:
        result = subprocess.run(
            ["git", "-C", str(self.checkout), *args], capture_output=True, text=True,
            check=False, timeout=10,
        )
        if result.returncode != 0:
            raise AssertionError(f"git {args[0]} failed: {result.stderr}")
        return result.stdout.strip()

    def _make_subject(self, *, migration: bool, canary: Path | None) -> None:
        self.checkout.mkdir()
        subprocess.run(["git", "init", "-q", str(self.checkout)], check=True, capture_output=True)
        self._git("config", "user.name", "Artifact Tests")
        self._git("config", "user.email", "artifact-tests@example.invalid")
        self._git("config", "remote.origin.url", f"https://github.com/{REPOSITORY}.git")
        scripts = self.checkout / "scripts"
        scripts.mkdir()
        self.media_base = b"base media producer helper\n"
        self.ui_base = b"base UI producer helper\n"
        (scripts / "media-network-e2e.py").write_bytes(self.media_base)
        (scripts / "ui-e2e.py").write_bytes(self.ui_base)
        (self.checkout / "README.md").write_text("base\n", encoding="utf-8")
        self._git("add", "-A")
        self._git("commit", "-qm", "base")
        self.base = self._git("rev-parse", "HEAD")
        self.producer_ui = self.ui_base
        if migration:
            self.producer_ui = (
                f"from pathlib import Path\nPath({str(canary)!r}).write_text('candidate-imported')\n".encode()
            )
            (scripts / "ui-e2e.py").write_bytes(self.producer_ui)
        (self.checkout / "README.md").write_text("feature\n", encoding="utf-8")
        self._git("add", "-A")
        self._git("commit", "-qm", "feature")
        self.head = self._git("rev-parse", "HEAD")
        self.tree = self._git("rev-parse", "HEAD^{tree}")

    def _make_control(self) -> None:
        self.control.mkdir()
        sources = {
            "verify_ci_artifacts.py": SCRIPTS / "verify_ci_artifacts.py",
            "artifact_validation.py": SCRIPTS / "artifact_validation.py",
            "media-network-e2e.py": SCRIPTS / "media-network-e2e.py",
            "ui-e2e.py": SCRIPTS / "ui-e2e.py",
        }
        files: dict[str, str] = {}
        for name, source in sources.items():
            shutil.copyfile(source, self.control / name)
            files[name] = _digest((self.control / name).read_bytes())
        self.release = "test-release-1"
        self.control_manifest = {"schema_version": 1, "release_id": self.release, "files": files}
        _write_json(self.control / "control-manifest.json", self.control_manifest)
        self.control_digest = _digest((self.control / "control-manifest.json").read_bytes())
        for path in self.control.iterdir():
            path.chmod(0o444)
        self.control.chmod(0o555)

    def _make_evidence(self) -> dict[str, Any]:
        with tempfile.TemporaryDirectory(dir=self.root) as directory:
            native_zip = _native_archive(Path(directory))
        archives = {
            "native": native_zip,
            "macos": _ui_archive("macos"),
            "tvos": _ui_archive("tvos"),
        }
        run_ids = {"ci": 433, "macos": 544, "tvos": 544}
        jobs = {
            "ci": [
                {"id": 601, "name": "Native media network acceptance", "status": "completed", "conclusion": "success"},
                {"id": 602, "name": "Build and unit tests", "status": "completed", "conclusion": "success"},
            ],
            "macos": [
                {"id": 701, "name": "macos UI playback E2E", "status": "completed", "conclusion": "success"},
                {"id": 702, "name": "macOS build", "status": "completed", "conclusion": "success"},
            ],
            "tvos": [
                {"id": 703, "name": "tvos UI playback E2E", "status": "completed", "conclusion": "success"},
                {"id": 704, "name": "tvOS build", "status": "completed", "conclusion": "success"},
            ],
        }
        run_refs: dict[str, dict[str, Any]] = {}
        for key in ("ci", "macos", "tvos"):
            kind = "native" if key == "ci" else key
            run_ref = {
                "file": f"{key}-run.json", "run_id": run_ids[key], "run_attempt": 1,
                "workflow_name": "CI" if key == "ci" else "UI E2E",
                "job_id": jobs[key][0]["id"],
                "job_name": jobs[key][0]["name"],
                "jobs_file": f"{key}-jobs.json", "jobs_page_count": 1, "jobs_total_count": 2,
            }
            run_refs[key] = run_ref
            run_record = {
                "repository": REPOSITORY, "run_id": run_ids[key], "run_attempt": 1,
                "workflow_name": run_ref["workflow_name"], "status": "completed", "conclusion": "success",
                "head_sha": self.head,
                "pull_request": {"number": 42, "base_sha": self.base, "head_sha": self.head},
                "jobs_page_count": 1, "jobs_total_count": 2, "jobs": jobs[key],
                "job_id": run_ref["job_id"], "job_name": run_ref["job_name"],
                "job_status": "completed", "job_conclusion": "success",
                "subject": {"kind": "feature-head", "feature_head_sha": self.head},
            }
            _write_json(self.input / run_ref["file"], run_record)
            _write_json(self.input / run_ref["jobs_file"], {"total_count": 2, "jobs": jobs[key]})
        artifact_refs: dict[str, dict[str, Any]] = {}
        for index, kind in enumerate(("native", "macos", "tvos"), start=1):
            run_key = "ci" if kind == "native" else kind
            run_id = run_ids[run_key]
            artifact_id = 900 + index
            artifact_name = {
                "native": f"media-network-e2e-{run_id}",
                "macos": f"ui-e2e-macos-{run_id}",
                "tvos": f"ui-e2e-tvos-{run_id}",
            }[kind]
            archive_name = f"{kind}.zip"
            archive_raw = archives[kind]
            (self.input / archive_name).write_bytes(archive_raw)
            metadata_name = f"{kind}-metadata.json"
            expiry = (datetime.now(timezone.utc) + timedelta(days=3)).isoformat().replace("+00:00", "Z")
            archive_digest = _digest(archive_raw)
            artifact = {
                "id": artifact_id, "name": artifact_name, "expired": False, "expires_at": expiry,
                "workflow_run": {
                    "id": run_id, "head_sha": self.head,
                    "repository_id": REPOSITORY_ID, "head_repository_id": REPOSITORY_ID,
                },
                "archive_download_url": f"https://api.github.com/repos/{REPOSITORY}/actions/artifacts/{artifact_id}/zip",
                "size_in_bytes": len(archive_raw), "digest": f"sha256:{archive_digest}",
            }
            _write_json(self.input / metadata_name, {"total_count": 1, "artifacts": [artifact]})
            artifact_refs[kind] = {
                "metadata_file": metadata_name, "archive_file": archive_name,
                "artifact_id": artifact_id, "artifact_name": artifact_name,
                "run_id": run_id, "sha256": archive_digest, "size_in_bytes": len(archive_raw),
            }
        _write_json(self.input / "final-readback.json", {
            "pr_scope": "parent-final-exact-scope",
            "ci_run_id": run_refs["ci"]["run_id"], "ui_run_id": run_refs["macos"]["run_id"],
            "ci_jobs": 2, "ui_jobs": 2, "ci_job_pages": 1, "ui_job_pages": 1,
            "run_records_jobs_artifact_metadata_final_rereads": "typed-scope-fields-equal",
        })
        producer_helpers = {
            HELPERS[0]: _digest(self.media_base),
            HELPERS[1]: _digest(self.producer_ui),
        }
        route = "helper-migration" if self.producer_ui != self.ui_base else "ordinary"
        migration = None
        if route == "helper-migration":
            migration = {
                "path": HELPERS[1], "from_sha256": _digest(self.ui_base),
                "to_sha256": _digest(self.producer_ui), "report_contract": "native3-ui5-fixture4",
            }
        scope = {
            "schema_version": 1, "repository": REPOSITORY, "repository_id": REPOSITORY_ID,
            "pr_number": 42, "base_sha": self.base, "feature_head_sha": self.head,
            "feature_tree_sha": self.tree, "route": route,
            "producer_helpers": producer_helpers, "runs": run_refs,
            "artifacts": artifact_refs, "migration": migration,
        }
        manifest = {key: scope[key] for key in MANIFEST_FIELDS}
        _write_json(self.input / "manifest.json", manifest)
        return scope

    def run(self, *, output_name: str = "derived-output") -> subprocess.CompletedProcess[str]:
        output = self.root / output_name
        return subprocess.run(
            [sys.executable, "-I", "-S", str(self.control / "verify_ci_artifacts.py"),
             "--scope", str(self.scope_path), "--checkout", str(self.checkout),
             "--control-root", str(self.control), "--control-release", self.release,
             "--control-manifest-sha256", self.control_digest,
             "--input-root", str(self.input), "--output-root", str(output)],
            cwd=self.root, capture_output=True, text=True, check=False, timeout=30,
        )

    def raw_hashes(self) -> dict[str, str]:
        return {path.relative_to(self.input).as_posix(): _digest(path.read_bytes())
                for path in sorted(self.input.rglob("*")) if path.is_file()}

    def rewrite_scope_and_manifest(self) -> None:
        _write_json(self.scope_path, self.scope)
        _write_json(self.input / "manifest.json", {key: self.scope[key] for key in MANIFEST_FIELDS})

    def replace_archive(self, kind: str, changes: dict[str, bytes]) -> None:
        ref = self.scope["artifacts"][kind]
        archive_path = self.input / ref["archive_file"]
        with zipfile.ZipFile(archive_path, "r") as archive:
            members = {name: archive.read(name) for name in archive.namelist()}
        members.update(changes)
        stream = BytesIO()
        with zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, raw in sorted(members.items()):
                archive.writestr(name, raw)
        raw = stream.getvalue()
        archive_path.write_bytes(raw)
        ref["sha256"] = _digest(raw)
        ref["size_in_bytes"] = len(raw)
        metadata_path = self.input / ref["metadata_file"]
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        selected = next(item for item in metadata["artifacts"] if item["id"] == ref["artifact_id"])
        selected["size_in_bytes"] = len(raw)
        selected["digest"] = f"sha256:{_digest(raw)}"
        _write_json(metadata_path, metadata)
        self.rewrite_scope_and_manifest()


class VerifyCIArtifactsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.harness = Harness()

    def tearDown(self) -> None:
        self.harness.close()

    def assert_rejected(self, result: subprocess.CompletedProcess[str], output: Path) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((output / "proof.json").exists())
        self.assertTrue((output / "failure.json").is_file())

    def test_entrypoint_loads_core_bytes_captured_during_bundle_hashing(self) -> None:
        entrypoint_path = SCRIPTS / "verify_ci_artifacts.py"
        spec = importlib.util.spec_from_file_location("verify_ci_artifacts_test", entrypoint_path)
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader)
        entrypoint = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(entrypoint)
        manifest, verified_core = entrypoint._check_bundle(
            self.harness.control, self.harness.release, self.harness.control_digest,
        )
        core_path = self.harness.control / "artifact_validation.py"
        self.harness.control.chmod(0o755)
        core_path.chmod(0o644)
        core_path.write_text("CORE_REPLACEMENT_CANARY = True\n", encoding="utf-8")
        core_path.chmod(0o444)
        self.harness.control.chmod(0o555)

        core = entrypoint._load_validation_core(self.harness.control, verified_core)
        self.assertEqual(core.REPORT_CONTRACT, "native3-ui5-fixture4")
        self.assertFalse(hasattr(core, "CORE_REPLACEMENT_CANARY"))
        self.assertEqual(manifest["files"]["artifact_validation.py"], _digest(verified_core))

    def test_synthetic_ordinary_scope_passes_and_records_separate_identities(self) -> None:
        before = self.harness.raw_hashes()
        result = self.harness.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        proof = json.loads((self.harness.output / "proof.json").read_text(encoding="utf-8"))
        self.assertEqual(proof["producer_source"]["route"], "ordinary")
        self.assertEqual(proof["producer_source"]["helpers"][HELPERS[0]]["base"],
                         proof["producer_source"]["helpers"][HELPERS[0]]["head"])
        self.assertEqual(proof["validator_control"]["release_id"], self.harness.release)
        self.assertEqual(proof["validator_control"]["manifest_sha256"], self.harness.control_digest)
        self.assertEqual(set(proof["artifacts"]), {"native", "macos", "tvos"})
        self.assertEqual(len(proof["artifacts"]["native"]["raw_metrics"]), 3)
        for platform in ("macos", "tvos"):
            self.assertEqual(len(proof["artifacts"][platform]["named_case_counts"]), 5)
            self.assertEqual(set(proof["artifacts"][platform]["fixture_metrics"]["scenarios"]),
                             {"automatic", "manual", "silent", "resumption"})
        self.assertEqual(self.harness.raw_hashes(), before)

    def test_helper_migration_uses_control_bundle_and_never_imports_candidate_helper(self) -> None:
        marker = self.harness.root / "candidate-imported.txt"
        self.harness.close()
        self.harness = Harness(migration=True, canary=marker)
        before = self.harness.raw_hashes()
        result = self.harness.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists())
        proof = json.loads((self.harness.output / "proof.json").read_text(encoding="utf-8"))
        self.assertEqual(proof["producer_source"]["route"], "helper-migration")
        self.assertEqual(proof["producer_source"]["helpers"][HELPERS[1]]["base"],
                         self.harness.scope["migration"]["from_sha256"])
        self.assertEqual(proof["producer_source"]["helpers"][HELPERS[1]]["head"],
                         self.harness.scope["migration"]["to_sha256"])
        self.assertEqual(proof["validator_control"]["files"]["ui-e2e.py"],
                         self.harness.control_manifest["files"]["ui-e2e.py"])
        self.assertEqual(self.harness.raw_hashes(), before)

    def test_ordinary_route_rejects_a_changed_helper(self) -> None:
        self.harness.close()
        self.harness = Harness(migration=True, canary=self.harness.root / "candidate-imported.txt")
        self.harness.scope["route"] = "ordinary"
        self.harness.scope["migration"] = None
        self.harness.rewrite_scope_and_manifest()
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("changed range", result.stderr)
        self.assertTrue((self.harness.output / "failure.json").is_file())

    def test_manifest_scope_mismatch_is_rejected_without_changing_raw_input(self) -> None:
        before = self.harness.raw_hashes()
        manifest_path = self.harness.input / "manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["feature_head_sha"] = "0" * 40
        _write_json(manifest_path, manifest)
        before_failure = self.harness.raw_hashes()
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("exactly match trusted scope", result.stderr)
        self.assertEqual(self.harness.raw_hashes(), before_failure)
        self.assertNotEqual(before, before_failure)

    def test_manifest_must_match_every_operator_assigned_identity_and_pin(self) -> None:
        baseline = {key: self.harness.scope[key] for key in MANIFEST_FIELDS}
        mutations = (
            ("repository", ("repository",), "Another/repository"),
            ("repository_id", ("repository_id",), REPOSITORY_ID + 1),
            ("pr_number", ("pr_number",), 43),
            ("base_sha", ("base_sha",), "1" * 40),
            ("head_sha", ("feature_head_sha",), "2" * 40),
            ("tree_sha", ("feature_tree_sha",), "3" * 40),
            ("run_id", ("runs", "ci", "run_id"), 434),
            ("run_attempt", ("runs", "ci", "run_attempt"), 2),
            ("job_id", ("runs", "ci", "job_id"), 699),
            ("job_pages", ("runs", "ci", "jobs_page_count"), 2),
            ("artifact_id", ("artifacts", "native", "artifact_id"), 999),
            ("artifact_digest", ("artifacts", "native", "sha256"), "0" * 64),
            ("artifact_size", ("artifacts", "native", "size_in_bytes"), 2),
        )
        for index, (label, path, value) in enumerate(mutations):
            with self.subTest(field=label):
                manifest = json.loads(_json_bytes(baseline))
                selected = manifest
                for key in path[:-1]:
                    selected = selected[key]
                selected[path[-1]] = value
                _write_json(self.harness.input / "manifest.json", manifest)
                result = self.harness.run(output_name=f"scope-reject-{index}")
                self.assert_rejected(result, self.harness.root / f"scope-reject-{index}")
                self.assertIn("exactly match trusted scope", result.stderr)
        _write_json(self.harness.input / "manifest.json", baseline)

    def test_trusted_artifact_hash_and_size_pins_must_match_the_archive(self) -> None:
        original = json.loads(_json_bytes(self.harness.scope))
        cases = (("sha256", "0" * 64), ("size_in_bytes", original["artifacts"]["native"]["size_in_bytes"] + 1))
        for index, (field, value) in enumerate(cases):
            with self.subTest(field=field):
                self.harness.scope = json.loads(_json_bytes(original))
                self.harness.scope["artifacts"]["native"][field] = value
                self.harness.rewrite_scope_and_manifest()
                result = self.harness.run(output_name=f"artifact-pin-reject-{index}")
                self.assert_rejected(result, self.harness.root / f"artifact-pin-reject-{index}")
                self.assertIn("archive size/digest", result.stderr)
        self.harness.scope = original
        self.harness.rewrite_scope_and_manifest()

    def test_authenticated_metadata_digest_must_match_archive_bytes(self) -> None:
        ref = self.harness.scope["artifacts"]["native"]
        path = self.harness.input / ref["metadata_file"]
        metadata = json.loads(path.read_text(encoding="utf-8"))
        metadata["artifacts"][0]["digest"] = "sha256:" + "0" * 64
        _write_json(path, metadata)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("metadata digest mismatch", result.stderr)

    def test_incomplete_jobs_page_is_rejected(self) -> None:
        path = self.harness.input / "ci-jobs.json"
        jobs = json.loads(path.read_text(encoding="utf-8"))
        jobs["total_count"] = 3
        _write_json(path, jobs)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("job collection incomplete", result.stderr)

    def test_wrong_run_attempt_is_rejected(self) -> None:
        path = self.harness.input / "ci-run.json"
        run = json.loads(path.read_text(encoding="utf-8"))
        run["run_attempt"] = 2
        _write_json(path, run)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("not exact completed success", result.stderr)

    def test_run_head_must_match_exact_feature_or_synthetic_subject(self) -> None:
        path = self.harness.input / "ci-run.json"
        run = json.loads(path.read_text(encoding="utf-8"))
        run["head_sha"] = "4" * 40
        _write_json(path, run)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("synthetic-check attribution", result.stderr)

    def test_producer_worktree_helper_must_match_the_pinned_head_blob(self) -> None:
        helper = self.harness.checkout / HELPERS[1]
        helper.write_bytes(helper.read_bytes() + b"\nworktree drift\n")
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("worktree/base/head", result.stderr)

    def test_expired_artifact_is_rejected(self) -> None:
        ref = self.harness.scope["artifacts"]["native"]
        path = self.harness.input / ref["metadata_file"]
        metadata = json.loads(path.read_text(encoding="utf-8"))
        metadata["artifacts"][0]["expires_at"] = "2000-01-01T00:00:00Z"
        _write_json(path, metadata)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("artifact expired", result.stderr)

    def test_raw_artifact_symlink_is_rejected(self) -> None:
        archive = self.harness.input / self.harness.scope["artifacts"]["native"]["archive_file"]
        outside = self.harness.root / "outside.zip"
        outside.write_bytes(archive.read_bytes())
        archive.unlink()
        archive.symlink_to(outside)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("unsafe non-regular tree entry", result.stderr)

    def test_tampered_and_undeclared_control_files_fail_before_proof(self) -> None:
        ui_path = self.harness.control / "ui-e2e.py"
        ui_path.chmod(0o644)
        ui_path.write_bytes(ui_path.read_bytes() + b"\n# tampered\n")
        ui_path.chmod(0o444)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("control file hash mismatch", result.stderr)

    def test_unknown_control_cache_is_rejected(self) -> None:
        self.harness.close()
        self.harness = Harness()
        self.harness.control.chmod(0o755)
        (self.harness.control / "__pycache__").mkdir()
        (self.harness.control / "__pycache__" / "unexpected.pyc").write_bytes(b"cache")
        self.harness.control.chmod(0o555)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("unexpected control directory", result.stderr)

    def test_changed_native_deadline_is_rejected_by_released_assertions(self) -> None:
        with tempfile.TemporaryDirectory(dir=self.harness.root) as directory:
            files, _ = _native_report_files(Path(directory))
        payload = json.loads(files["rust/abr.json"].decode("utf-8"))
        payload["probe"]["overall_deadline_seconds"] = 999
        self.harness.replace_archive("native", {
            "rust/abr.json": _json_bytes(payload),
        })
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("native report validation failed", result.stderr)

    def test_skipped_ui_case_is_rejected_by_released_assertions(self) -> None:
        with zipfile.ZipFile(self.harness.input / "macos.zip", "r") as archive:
            members = {name: archive.read(name) for name in archive.namelist()}
        payload = json.loads(members["Reports/xcresult-tests.json"].decode("utf-8"))
        payload["testNodes"][0]["children"][0]["result"] = "Skipped"
        members["Reports/xcresult-tests.json"] = _json_bytes(payload)
        self.harness.replace_archive("macos", members)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("skipped UI tests", result.stderr)

    def test_invalid_fixture_metric_is_rejected_by_released_assertions(self) -> None:
        with zipfile.ZipFile(self.harness.input / "tvos.zip", "r") as archive:
            members = {name: archive.read(name) for name in archive.namelist()}
        metrics = json.loads(members["Reports/task-fixture-metrics.json"].decode("utf-8"))
        metrics["scenarios"]["manual"]["create_count"] = 2
        members["Reports/task-fixture-metrics.json"] = _json_bytes(metrics)
        self.harness.replace_archive("tvos", members)
        result = self.harness.run()
        self.assert_rejected(result, self.harness.output)
        self.assertIn("exactly one task", result.stderr)


if __name__ == "__main__":
    unittest.main()
