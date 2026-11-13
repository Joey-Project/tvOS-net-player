#!/usr/bin/env python3
"""Validate exact-scope CI artifacts with an independently pinned control bundle."""

from __future__ import annotations

import hashlib
import importlib.machinery
import importlib.util
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import types
import zipfile
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import Any


SCHEMA_VERSION = 1
CONTROL_MANIFEST = "control-manifest.json"
CONTROL_FILES = (
    "verify_ci_artifacts.py",
    "artifact_validation.py",
    "media-network-e2e.py",
    "ui-e2e.py",
)
HELPERS = ("scripts/media-network-e2e.py", "scripts/ui-e2e.py")
KINDS = ("native", "macos", "tvos")
RUN_KEYS = ("ci", "macos", "tvos")
EXPECTED_JOBS = {
    "native": "Native media network acceptance",
    "macos": "macos UI playback E2E",
    "tvos": "tvos UI playback E2E",
}
EXPECTED_WORKFLOWS = {"native": "CI", "macos": "UI E2E", "tvos": "UI E2E"}
ARTIFACT_PREFIXES = {"native": "media-network-e2e", "macos": "ui-e2e-macos", "tvos": "ui-e2e-tvos"}
RUN_REF_FIELDS = {
    "file", "run_id", "run_attempt", "workflow_name", "job_id", "job_name",
    "jobs_file", "jobs_page_count", "jobs_total_count",
}
ARTIFACT_REF_FIELDS = {
    "metadata_file", "archive_file", "artifact_id", "artifact_name", "run_id", "sha256", "size_in_bytes",
}
MANIFEST_FIELDS = {
    "schema_version", "repository", "repository_id", "pr_number", "base_sha",
    "feature_head_sha", "feature_tree_sha", "runs", "artifacts",
}
SCOPE_FIELDS = MANIFEST_FIELDS | {"route", "producer_helpers", "migration"}
RUN_RECORD_FIELDS = {
    "repository", "run_id", "run_attempt", "workflow_name", "status", "conclusion", "head_sha",
    "pull_request", "jobs_page_count", "jobs_total_count", "jobs", "job_id", "job_name",
    "job_status", "job_conclusion", "subject",
}
NATIVE_FILES = {
    "logs/cargo-compile.log", "logs/cargo-test.log", "logs/swift-compile.log",
    "logs/system-mute-set.log", "logs/system-mute-verify.log", "rust/abr.json",
    "rust/buffered.json", "rust/transcoding.json", "summary.json",
}
UI_JSON_NAMES = {
    "xcresult-summary.json", "xcresult-tests.json", "result-validation.json",
    "task-fixture-metrics.json", "task-fixture-validation.json",
}
MAX_ZIP = 16 * 1024 * 1024
MAX_RAW_ARCHIVES = 48 * 1024 * 1024
MAX_EXPANDED = 64 * 1024 * 1024
MAX_DIAGNOSTICS = 4 * 1024 * 1024
MAX_JSON = 1024 * 1024
MAX_ZIP_JSON = 4 * 1024 * 1024
MAX_NATIVE_MEMBER = 4 * 1024 * 1024
MAX_EVIDENCE_TREE = 64 * 1024 * 1024
MAX_EVIDENCE_FILES = 256
MAX_CONTROL_FILE = 8 * 1024 * 1024
MAX_CONTROL_TREE = 32 * 1024 * 1024
REPORT_CONTRACT = "native3-ui5-fixture4"


class VerificationError(RuntimeError):
    """A fail-closed evidence or control validation error."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise VerificationError(message)


def _json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise VerificationError(f"duplicate JSON object key: {key}")
        result[key] = value
    return result


def _parse_json(raw: bytes, label: str) -> Any:
    try:
        return json.loads(raw.decode("utf-8"), object_pairs_hook=_json_object)
    except VerificationError:
        raise
    except (UnicodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"invalid JSON: {label}") from error


def _identity(info: os.stat_result) -> tuple[int, int, int, int]:
    return (info.st_dev, info.st_ino, stat.S_IMODE(info.st_mode), info.st_nlink)


def _read_file(path: Path, limit: int, label: str) -> tuple[bytes, os.stat_result]:
    try:
        before = path.lstat()
    except FileNotFoundError as error:
        raise VerificationError(f"missing file: {label}") from error
    except OSError as error:
        raise VerificationError(f"unreadable file metadata: {label}") from error
    require(stat.S_ISREG(before.st_mode), f"unsafe non-regular file: {label}")
    require(before.st_nlink == 1, f"unsafe linked file: {label}")
    require(before.st_size <= limit, f"file exceeds byte limit: {label}")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except FileNotFoundError as error:
        raise VerificationError(f"missing file: {label}") from error
    except OSError as error:
        raise VerificationError(f"unreadable file: {label}") from error
    try:
        opened = os.fstat(descriptor)
        require(stat.S_ISREG(opened.st_mode) and _identity(opened) == _identity(before),
                f"file object changed while opening: {label}")
        chunks: list[bytes] = []
        total = 0
        while True:
            try:
                block = os.read(descriptor, min(1024 * 1024, limit + 1 - total))
            except OSError as error:
                raise VerificationError(f"unreadable file: {label}") from error
            if not block:
                break
            total += len(block)
            require(total <= limit, f"file exceeds byte limit: {label}")
            chunks.append(block)
        after = os.fstat(descriptor)
        require(_identity(after) == _identity(opened) and total == opened.st_size,
                f"file object or size changed while reading: {label}")
        return b"".join(chunks), after
    finally:
        os.close(descriptor)


def _read_json(path: Path, label: str, limit: int = MAX_JSON) -> Any:
    raw, _ = _read_file(path, limit, label)
    return _parse_json(raw, label)


def _safe_relative(relative: Any, label: str) -> tuple[str, ...]:
    require(isinstance(relative, str) and relative and "\\" not in relative and "\x00" not in relative,
            f"unsafe relative path: {label}")
    parts = relative.split("/")
    require(all(part not in ("", ".", "..") for part in parts)
            and not relative.startswith("/") and re.match(r"^[A-Za-z]:", relative) is None,
            f"unsafe relative path: {label}")
    return tuple(parts)


def _input_path(root: Path, relative: Any) -> Path:
    parts = _safe_relative(relative, "evidence reference")
    current = root
    for part in parts:
        current = current / part
        try:
            info = current.lstat()
        except FileNotFoundError as error:
            raise VerificationError(f"missing evidence file: {relative}") from error
        except OSError as error:
            raise VerificationError(f"unreadable evidence path: {relative}") from error
        require(not stat.S_ISLNK(info.st_mode), f"symlink in evidence path: {relative}")
    require(current.resolve() == current and current.is_relative_to(root),
            f"evidence path escapes input root: {relative}")
    require(current.is_file(), f"missing evidence file: {relative}")
    return current


def _json_hash(value: str) -> bool:
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def _sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _typed_equal(actual: Any, expected: Any) -> bool:
    if type(actual) is not type(expected):
        return False
    if isinstance(expected, dict):
        return actual.keys() == expected.keys() and all(
            _typed_equal(actual[key], expected[key]) for key in expected
        )
    if isinstance(expected, list):
        return len(actual) == len(expected) and all(
            _typed_equal(left, right) for left, right in zip(actual, expected, strict=True)
        )
    return actual == expected


def _inventory(root: Path, *, readonly: bool, file_limit: int, tree_limit: int,
               allow_dirs: set[str] | None = None) -> dict[str, dict[str, Any]]:
    try:
        root_info = root.lstat()
    except FileNotFoundError as error:
        raise VerificationError(f"missing root: {root}") from error
    except OSError as error:
        raise VerificationError(f"unreadable root: {root}") from error
    require(stat.S_ISDIR(root_info.st_mode) and root.resolve() == root,
            f"root is not a canonical directory: {root}")
    if readonly:
        require(stat.S_IMODE(root_info.st_mode) & 0o222 == 0, f"control root is writable: {root}")
    records: dict[str, dict[str, Any]] = {}
    directories: dict[str, tuple[int, int, int, int]] = {}
    total = 0
    stack = [root]
    while stack:
        parent = stack.pop()
        try:
            entries = sorted(os.scandir(parent), key=lambda entry: entry.name)
        except OSError as error:
            raise VerificationError(f"unreadable directory: {parent.relative_to(root)}") from error
        for entry in entries:
            path = Path(entry.path)
            relative = path.relative_to(root).as_posix()
            _safe_relative(relative, "tree entry")
            try:
                info = entry.stat(follow_symlinks=False)
            except OSError as error:
                raise VerificationError(f"unreadable tree entry: {relative}") from error
            if stat.S_ISDIR(info.st_mode):
                directories[relative] = _identity(info)
                require(len(directories) <= MAX_EVIDENCE_FILES, "tree contains too many directories")
                if readonly:
                    require(stat.S_IMODE(info.st_mode) & 0o222 == 0, f"control directory is writable: {relative}")
                stack.append(path)
                continue
            require(stat.S_ISREG(info.st_mode), f"unsafe non-regular tree entry: {relative}")
            require(info.st_nlink == 1, f"unsafe linked tree entry: {relative}")
            if readonly:
                require(stat.S_IMODE(info.st_mode) & 0o222 == 0, f"control file is writable: {relative}")
            raw, opened = _read_file(path, file_limit, relative)
            require(_identity(info) == _identity(opened), f"file object changed during inventory: {relative}")
            total += len(raw)
            require(total <= tree_limit, "tree exceeds aggregate byte limit")
            records[relative] = {
                "identity": _identity(opened), "size": len(raw), "sha256": _sha256(raw),
            }
            require(len(records) <= MAX_EVIDENCE_FILES, "tree contains too many files")
    if allow_dirs is not None:
        require(set(directories) == allow_dirs, "control bundle directory layout mismatch")
    return {"root_identity": _identity(root_info), "directories": directories, "files": records}


def _check_control_bundle(root: Path, release_id: str, manifest_sha256: str) -> dict[str, Any]:
    require(isinstance(release_id, str) and bool(release_id.strip()), "control release identity is missing")
    require(_json_hash(manifest_sha256), "control manifest SHA-256 is malformed")
    snapshot = _inventory(root, readonly=True, file_limit=MAX_CONTROL_FILE,
                          tree_limit=MAX_CONTROL_TREE, allow_dirs=set())
    files = snapshot["files"]
    expected_names = set(CONTROL_FILES) | {CONTROL_MANIFEST}
    require(set(files) == expected_names, "control bundle contains missing or undeclared files")
    manifest_path = root / CONTROL_MANIFEST
    raw, _ = _read_file(manifest_path, MAX_JSON, CONTROL_MANIFEST)
    require(_sha256(raw) == manifest_sha256, "control manifest digest mismatch")
    manifest = _parse_json(raw, CONTROL_MANIFEST)
    require(isinstance(manifest, dict) and set(manifest) == {"schema_version", "release_id", "files"},
            "control manifest fields/schema mismatch")
    require(type(manifest["schema_version"]) is int and manifest["schema_version"] == SCHEMA_VERSION,
            "unsupported control manifest schema")
    require(manifest["release_id"] == release_id, "control release identity mismatch")
    require(isinstance(manifest["files"], dict) and set(manifest["files"]) == set(CONTROL_FILES),
            "control manifest file list mismatch")
    for name in CONTROL_FILES:
        digest = manifest["files"][name]
        require(_json_hash(digest), f"control manifest hash is malformed: {name}")
        require(files[name]["sha256"] == digest, f"control file hash mismatch: {name}")
    return {"snapshot": snapshot, "manifest": manifest, "manifest_sha256": manifest_sha256}


def _same_control(before: dict[str, Any], after: dict[str, Any]) -> None:
    require(before["manifest_sha256"] == after["manifest_sha256"], "control manifest changed during validation")
    require(before["snapshot"] == after["snapshot"], "control bundle object, access policy, or content changed")


def _scope_file(path: Path) -> tuple[dict[str, Any], dict[str, Any]]:
    require(path.is_absolute() and path.resolve() == path, "scope path must be absolute and canonical")
    raw, info = _read_file(path, MAX_JSON, "trusted scope")
    scope = _parse_json(raw, "trusted scope")
    require(isinstance(scope, dict) and set(scope) == SCOPE_FIELDS, "trusted scope fields/schema mismatch")
    return scope, {"identity": _identity(info), "size": len(raw), "sha256": _sha256(raw)}


def _scope_identity(scope: Any) -> None:
    require(isinstance(scope, dict) and set(scope) == SCOPE_FIELDS, "trusted scope fields/schema mismatch")
    require(type(scope["schema_version"]) is int and scope["schema_version"] == SCHEMA_VERSION,
            "unsupported trusted scope schema")
    repository = scope["repository"]
    require(isinstance(repository, str) and re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is not None,
            "trusted repository name is invalid")
    for key in ("repository_id", "pr_number"):
        require(type(scope[key]) is int and scope[key] > 0, f"trusted {key} is invalid")
    for key in ("base_sha", "feature_head_sha", "feature_tree_sha"):
        require(isinstance(scope[key], str) and re.fullmatch(r"[0-9a-f]{40}", scope[key]) is not None,
                f"trusted {key} is invalid")
    require(scope["base_sha"] != scope["feature_head_sha"], "trusted base and feature head must differ")
    require(scope["route"] in ("ordinary", "helper-migration"), "unsupported helper validation route")
    helpers = scope["producer_helpers"]
    require(isinstance(helpers, dict) and set(helpers) == set(HELPERS), "producer helper pin set mismatch")
    for path, digest in helpers.items():
        require(_json_hash(digest), f"producer helper hash is malformed: {path}")
    migration = scope["migration"]
    if scope["route"] == "ordinary":
        require(migration is None, "ordinary route must not include a helper migration")
    else:
        required = {"path", "from_sha256", "to_sha256", "report_contract"}
        require(isinstance(migration, dict) and set(migration) == required,
                "helper migration fields/schema mismatch")
        require(migration["path"] in HELPERS, "helper migration path is not an acceptance helper")
        require(_json_hash(migration["from_sha256"]) and _json_hash(migration["to_sha256"]),
                "helper migration hashes are malformed")
        require(migration["from_sha256"] != migration["to_sha256"], "helper migration must change content")
        require(migration["to_sha256"] == helpers[migration["path"]],
                "helper migration target differs from the producer helper pin")
        require(migration["report_contract"] == REPORT_CONTRACT, "helper migration changes the report contract")
    require(isinstance(scope["runs"], dict) and set(scope["runs"]) == set(RUN_KEYS),
            "trusted run reference set mismatch")
    require(isinstance(scope["artifacts"], dict) and set(scope["artifacts"]) == set(KINDS),
            "trusted artifact reference set mismatch")
    for key in RUN_KEYS:
        ref = scope["runs"][key]
        require(isinstance(ref, dict) and set(ref) == RUN_REF_FIELDS, f"trusted {key} run reference schema mismatch")
        _safe_relative(ref["file"], f"{key} run file")
        _safe_relative(ref["jobs_file"], f"{key} jobs file")
        require(type(ref["run_id"]) is int and ref["run_id"] > 0
                and type(ref["run_attempt"]) is int and ref["run_attempt"] > 0
                and type(ref["job_id"]) is int and ref["job_id"] > 0
                and type(ref["jobs_page_count"]) is int and ref["jobs_page_count"] == 1
                and type(ref["jobs_total_count"]) is int and ref["jobs_total_count"] > 0,
                f"trusted {key} run/job identity is invalid")
        kind = "native" if key == "ci" else key
        require(ref["workflow_name"] == EXPECTED_WORKFLOWS[kind] and ref["job_name"] == EXPECTED_JOBS[kind],
                f"trusted {key} workflow/job contract mismatch")
    for kind in KINDS:
        ref = scope["artifacts"][kind]
        require(isinstance(ref, dict) and set(ref) == ARTIFACT_REF_FIELDS,
                f"trusted {kind} artifact reference schema mismatch")
        _safe_relative(ref["metadata_file"], f"{kind} metadata file")
        _safe_relative(ref["archive_file"], f"{kind} archive file")
        run_key = "ci" if kind == "native" else kind
        run_ref = scope["runs"][run_key]
        require(type(ref["artifact_id"]) is int and ref["artifact_id"] > 0
                and type(ref["run_id"]) is int and ref["run_id"] == run_ref["run_id"]
                and type(ref["size_in_bytes"]) is int and 0 < ref["size_in_bytes"] <= MAX_ZIP
                and _json_hash(ref["sha256"]), f"trusted {kind} artifact identity is invalid")
        require(ref["artifact_name"] == f"{ARTIFACT_PREFIXES[kind]}-{run_ref['run_id']}",
                f"trusted {kind} artifact name differs from the acceptance contract")


def _validate_manifest(manifest: Any, scope: dict[str, Any], input_root: Path) -> dict[str, Any]:
    require(isinstance(manifest, dict) and set(manifest) == MANIFEST_FIELDS,
            "evidence manifest fields/schema mismatch")
    expected = {key: scope[key] for key in MANIFEST_FIELDS}
    require(_typed_equal(manifest, expected), "evidence manifest does not exactly match trusted scope")
    for key in RUN_KEYS:
        ref = manifest["runs"][key]
        _input_path(input_root, ref["file"])
        _input_path(input_root, ref["jobs_file"])
    for kind in KINDS:
        ref = manifest["artifacts"][kind]
        _input_path(input_root, ref["metadata_file"])
        _input_path(input_root, ref["archive_file"])
    return manifest


def _git(root: Path, *args: str, text: bool = True) -> str | bytes:
    git_path = shutil.which("git")
    require(git_path is not None, "git executable is unavailable")
    env = {
        "PATH": os.defpath, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_OPTIONAL_LOCKS": "0", "GIT_TERMINAL_PROMPT": "0", "LC_ALL": "C",
    }
    command = [git_path, "--no-pager", "-C", str(root), "-c", "core.fsmonitor=false",
               "-c", "core.hooksPath=/dev/null", "-c", "diff.external=", *args]
    try:
        result = subprocess.run(command, capture_output=True, check=False, env=env, timeout=20)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise VerificationError(f"git command failed or timed out: {args[0]}") from error
    require(len(result.stdout) <= 8 * 1024 * 1024, f"git output exceeds byte limit: {args[0]}")
    require(result.returncode == 0, f"git {args[0]} failed: {result.stderr.decode('utf-8', 'replace')[:500]}")
    if text:
        return result.stdout.decode("utf-8", "strict").strip()
    return result.stdout


def _remote_repository(url: str) -> str | None:
    match = re.fullmatch(
        r"(?:https?://github\.com/|git@github\.com:|ssh://git@github\.com/)([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?)(?:\.git)?/?",
        url.strip(), re.IGNORECASE,
    )
    return match.group(1).lower() if match else None


def _git_blob(root: Path, commit: str, relative: str) -> bytes:
    result = _git(root, "show", f"{commit}:{relative}", text=False)
    require(isinstance(result, bytes), f"git blob has unexpected result type: {relative}")
    return result


def _validate_checkout(root: Path, scope: dict[str, Any]) -> dict[str, Any]:
    require(root.is_absolute() and root.resolve() == root and root.is_dir(),
            "subject checkout must be an existing canonical directory")
    top = _git(root, "rev-parse", "--show-toplevel")
    require(top == str(root), "subject path is not the Git worktree root")
    remote = _git(root, "config", "--get", "remote.origin.url")
    require(_remote_repository(str(remote)) == scope["repository"].lower(),
            "subject checkout origin does not match trusted repository")
    require(_git(root, "rev-parse", "HEAD") == scope["feature_head_sha"],
            "subject checkout HEAD does not match trusted feature head")
    require(_git(root, "rev-parse", "HEAD^{tree}") == scope["feature_tree_sha"],
            "subject checkout tree does not match trusted feature tree")
    ancestor = _git(root, "merge-base", "--is-ancestor", scope["base_sha"], scope["feature_head_sha"], text=False)
    require(ancestor == b"", "trusted base is not an ancestor of the feature head")
    changed_raw = _git(root, "diff", "--no-ext-diff", "--no-renames", "--name-only",
                       f"{scope['base_sha']}..{scope['feature_head_sha']}")
    changed = set(filter(None, str(changed_raw).splitlines()))
    blob_hashes: dict[str, dict[str, str]] = {}
    for relative in HELPERS:
        path = root / relative
        try:
            info = path.lstat()
        except FileNotFoundError as error:
            raise VerificationError(f"missing producer helper: {relative}") from error
        except OSError as error:
            raise VerificationError(f"unreadable producer helper: {relative}") from error
        require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1,
                f"unsafe producer helper: {relative}")
        work_raw, _ = _read_file(path, MAX_CONTROL_FILE, relative)
        base_digest = _sha256(_git_blob(root, scope["base_sha"], relative))
        head_digest = _sha256(_git_blob(root, scope["feature_head_sha"], relative))
        work_digest = _sha256(work_raw)
        blob_hashes[relative] = {"base": base_digest, "head": head_digest, "worktree": work_digest}
    if scope["route"] == "ordinary":
        for relative in HELPERS:
            values = blob_hashes[relative]
            require(relative not in changed, f"producer helper is in the changed range: {relative}")
            require(values["base"] == values["head"] == values["worktree"] == scope["producer_helpers"][relative],
                    f"producer helper pin differs across worktree/base/head: {relative}")
    else:
        migration = scope["migration"]
        migration_path = migration["path"]
        changed_helpers = set(HELPERS) & changed
        require(changed_helpers == {migration_path}, "helper migration changes an undeclared acceptance helper")
        for relative in HELPERS:
            values = blob_hashes[relative]
            if relative == migration_path:
                require(values["base"] == migration["from_sha256"]
                        and values["head"] == values["worktree"] == migration["to_sha256"],
                        f"declared helper migration differs from base/head/worktree: {relative}")
            else:
                require(relative not in changed
                        and values["base"] == values["head"] == values["worktree"] == scope["producer_helpers"][relative],
                        f"undeclared producer helper change: {relative}")
        require(scope["producer_helpers"][migration_path] == migration["to_sha256"],
                "producer helper pin differs from migration target")
    return {"repository": scope["repository"], "base_sha": scope["base_sha"],
            "feature_head_sha": scope["feature_head_sha"], "feature_tree_sha": scope["feature_tree_sha"],
            "route": scope["route"], "helpers": blob_hashes,
            "changed_helper_paths": sorted(set(HELPERS) & changed)}


def _validate_run(run: Any, ref: dict[str, Any], kind: str, scope: dict[str, Any], input_root: Path) -> str:
    require(isinstance(run, dict) and set(run) == RUN_RECORD_FIELDS, f"{kind} run record schema mismatch")
    run_key = "ci" if kind == "native" else kind
    trusted_ref = scope["runs"][run_key]
    job_page = _read_json(_input_path(input_root, ref["jobs_file"]), f"{kind} jobs page")
    require(ref["jobs_page_count"] == 1 and isinstance(job_page, dict)
            and type(job_page.get("total_count")) is int and isinstance(job_page.get("jobs"), list),
            f"{kind} malformed single-object job response")
    raw_jobs = job_page["jobs"]
    require(job_page["total_count"] == ref["jobs_total_count"] == len(raw_jobs),
            f"{kind} job collection incomplete")
    raw_jobs_valid = all(
        isinstance(job, dict) and type(job.get("id")) is int
        and isinstance(job.get("name"), str) and isinstance(job.get("status"), str)
        and (job.get("conclusion") is None or isinstance(job.get("conclusion"), str))
        for job in raw_jobs
    )
    compact = [{"id": job["id"], "name": job["name"], "status": job["status"],
                "conclusion": job["conclusion"]} for job in raw_jobs if isinstance(job, dict)]
    require(raw_jobs_valid and compact == run.get("jobs"),
            f"{kind} normalized jobs differ from raw complete pages")
    require(run["repository"] == scope["repository"]
            and type(run["run_id"]) is int and run["run_id"] == trusted_ref["run_id"]
            and type(run["run_attempt"]) is int and run["run_attempt"] == trusted_ref["run_attempt"]
            and run["workflow_name"] == trusted_ref["workflow_name"] == EXPECTED_WORKFLOWS[kind]
            and run["status"] == "completed" and run["conclusion"] == "success"
            and type(run["jobs_page_count"]) is int and run["jobs_page_count"] == trusted_ref["jobs_page_count"]
            and type(run["jobs_total_count"]) is int and run["jobs_total_count"] == trusted_ref["jobs_total_count"]
            and isinstance(run["jobs"], list) and len(run["jobs"]) == trusted_ref["jobs_total_count"]
            and all(isinstance(job, dict) and set(job) == {"id", "name", "status", "conclusion"}
                    and type(job["id"]) is int and job["status"] == "completed"
                    and job["conclusion"] == "success" for job in run["jobs"])
            and len({job["id"] for job in run["jobs"]}) == len(run["jobs"])
            and type(run["job_id"]) is int and run["job_id"] == trusted_ref["job_id"]
            and run["job_name"] == trusted_ref["job_name"] == EXPECTED_JOBS[kind]
            and run["job_status"] == "completed" and run["job_conclusion"] == "success",
            f"{kind} run/job is not exact completed success")
    selected = [job for job in run["jobs"] if job["id"] == run["job_id"]]
    require(len(selected) == 1 and selected[0]["name"] == run["job_name"],
            f"{kind} selected job differs from complete job list")
    require(isinstance(run["head_sha"], str) and re.fullmatch(r"[0-9a-f]{40}", run["head_sha"]) is not None,
            f"{kind} run head SHA is invalid")
    pr = run["pull_request"]
    expected_pr = {"number": scope["pr_number"], "base_sha": scope["base_sha"],
                   "head_sha": scope["feature_head_sha"]}
    require(isinstance(pr, dict) and set(pr) == set(expected_pr)
            and _typed_equal(pr, expected_pr),
            f"{kind} run is not associated with the assigned PR/base/feature head")
    subject = run["subject"]
    if run["head_sha"] == scope["feature_head_sha"]:
        require(subject == {"kind": "feature-head", "feature_head_sha": scope["feature_head_sha"]},
                f"{kind} feature subject mismatch")
    else:
        require(isinstance(subject, dict)
                and set(subject) == {"kind", "feature_head_sha", "synthetic_sha", "check_name"}
                and subject["kind"] == "synthetic-check"
                and subject["feature_head_sha"] == scope["feature_head_sha"]
                and subject["synthetic_sha"] == run["head_sha"]
                and isinstance(subject["check_name"], str) and bool(subject["check_name"].strip()),
                f"{kind} non-feature run lacks explicit synthetic-check attribution")
    return run["head_sha"]


def _validate_expiry(value: Any, kind: str, now: datetime | None = None) -> datetime:
    require(isinstance(value, str), f"{kind} artifact expiry missing")
    try:
        expires = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise VerificationError(f"{kind} artifact expiry is invalid") from error
    require(expires.tzinfo is not None, f"{kind} artifact expiry is timezone-free")
    require(expires > (now or datetime.now(timezone.utc)), f"{kind} artifact expired")
    return expires


def _validate_artifact(ref: dict[str, Any], run_sha: str, scope: dict[str, Any],
                       input_root: Path, kind: str) -> tuple[Path, dict[str, Any]]:
    metadata = _read_json(_input_path(input_root, ref["metadata_file"]), f"{kind} artifact metadata")
    require(isinstance(metadata, dict) and isinstance(metadata.get("artifacts"), list)
            and type(metadata.get("total_count")) is int
            and metadata["total_count"] == len(metadata["artifacts"])
            and len(metadata["artifacts"]) <= 100,
            f"{kind} artifact metadata incomplete or malformed")
    selected = [item for item in metadata["artifacts"]
                if isinstance(item, dict) and type(item.get("id")) is int and item.get("id") == ref["artifact_id"]]
    require(len(selected) == 1, f"{kind} artifact ID missing or duplicated")
    item = selected[0]
    require(item.get("name") == ref["artifact_name"] and item.get("expired") is False,
            f"{kind} artifact name/expiry mismatch")
    expiry = item.get("expires_at")
    _validate_expiry(expiry, kind)
    workflow = item.get("workflow_run")
    require(isinstance(workflow, dict)
            and type(workflow.get("id")) is int and workflow["id"] == ref["run_id"]
            and workflow.get("head_sha") == run_sha
            and type(workflow.get("repository_id")) is int
            and workflow["repository_id"] == scope["repository_id"]
            and type(workflow.get("head_repository_id")) is int
            and workflow["head_repository_id"] == scope["repository_id"],
            f"{kind} artifact workflow binding mismatch")
    expected_url = f"https://api.github.com/repos/{scope['repository']}/actions/artifacts/{ref['artifact_id']}/zip"
    require(item.get("archive_download_url") == expected_url, f"{kind} artifact download URL mismatch")
    archive = _input_path(input_root, ref["archive_file"])
    raw, _ = _read_file(archive, MAX_ZIP, f"{kind} archive")
    digest = _sha256(raw)
    size = len(raw)
    require(size == ref["size_in_bytes"] and type(item.get("size_in_bytes")) is int
            and size == item["size_in_bytes"] and digest == ref["sha256"],
            f"{kind} archive size/digest does not match authenticated metadata and trusted scope")
    require(item.get("digest") == f"sha256:{digest}", f"{kind} metadata digest mismatch")
    meta = {"artifact_id": ref["artifact_id"], "artifact_name": ref["artifact_name"],
            "run_id": ref["run_id"], "run_head_sha": run_sha, "sha256": digest,
            "size_in_bytes": size, "expires_at": expiry}
    return archive, meta


def _final_readback(input_root: Path, scope: dict[str, Any]) -> dict[str, Any]:
    final = _read_json(_input_path(input_root, "final-readback.json"), "final exact-scope readback")
    ci = scope["runs"]["ci"]
    ui = scope["runs"]["macos"]
    require(isinstance(final, dict)
            and final.get("pr_scope") == "parent-final-exact-scope"
            and type(final.get("ci_run_id")) is int
            and final.get("ci_run_id") == ci["run_id"]
            and type(final.get("ui_run_id")) is int
            and final.get("ui_run_id") == ui["run_id"]
            and type(final.get("ci_jobs")) is int
            and final.get("ci_jobs") == ci["jobs_total_count"]
            and type(final.get("ui_jobs")) is int
            and final.get("ui_jobs") == ui["jobs_total_count"]
            and type(final.get("ci_job_pages")) is int
            and final.get("ci_job_pages") == ci["jobs_page_count"]
            and type(final.get("ui_job_pages")) is int
            and final.get("ui_job_pages") == ui["jobs_page_count"]
            and final.get("run_records_jobs_artifact_metadata_final_rereads") == "typed-scope-fields-equal",
            "parent final scope/run/job/artifact reread does not match selected evidence")
    return final


def _inspect_zip(archive: Path, *, kind: str, expansion_cap: int) -> tuple[zipfile.ZipFile, dict[str, zipfile.ZipInfo]]:
    raw, _ = _read_file(archive, MAX_ZIP, f"{kind} archive")
    from io import BytesIO

    try:
        zf = zipfile.ZipFile(BytesIO(raw))
        infos = zf.infolist()
    except (OSError, zipfile.BadZipFile) as error:
        raise VerificationError(f"{kind} artifact is not a valid ZIP") from error
    try:
        require(len(infos) <= (64 if kind == "native" else 4096), f"{kind} ZIP entry count exceeds its cap")
        seen: set[str] = set()
        folded: set[str] = set()
        members: dict[str, zipfile.ZipInfo] = {}
        expanded = 0
        for info in infos:
            name = info.filename
            require(isinstance(name, str) and name and "\\" not in name and not name.startswith("/"),
                    f"{kind} ZIP contains an unsafe member name")
            is_directory = info.is_dir()
            normalized_name = name[:-1] if is_directory else name
            require("\x00" not in name and re.match(r"^[A-Za-z]:", name) is None,
                    f"{kind} ZIP contains an absolute or NUL member name")
            parts = normalized_name.split("/")
            require(all(part not in ("", ".", "..") for part in parts),
                    f"{kind} ZIP contains traversal/empty path components")
            normalized = PurePosixPath(normalized_name).as_posix()
            require(normalized == normalized_name and normalized not in seen and normalized.casefold() not in folded,
                    f"{kind} ZIP contains duplicate or case-colliding paths")
            seen.add(normalized)
            folded.add(normalized.casefold())
            require(not (info.flag_bits & 0x1), f"{kind} ZIP contains an encrypted member")
            mode = (info.external_attr >> 16) & 0xFFFF
            file_type = stat.S_IFMT(mode)
            allowed_types = (0, stat.S_IFDIR) if is_directory else (0, stat.S_IFREG)
            require(file_type in allowed_types and (kind != "native" or not is_directory),
                    f"{kind} ZIP contains a non-regular or symlink member: {name}")
            require(info.file_size >= 0 and info.compress_size >= 0, f"{kind} ZIP has invalid member sizes")
            expanded += info.file_size
            require(expanded <= expansion_cap, f"{kind} ZIP exceeds aggregate expansion cap")
            members[name] = info
        if kind == "native":
            require(set(members) == NATIVE_FILES, "native ZIP does not contain the exact nine required regular files")
            for name, info in members.items():
                require(info.file_size <= MAX_NATIVE_MEMBER, f"native ZIP member exceeds 4 MiB: {name}")
        streamed = 0
        try:
            for info in infos:
                member_bytes = 0
                with zf.open(info, "r") as source:
                    while block := source.read(64 * 1024):
                        member_bytes += len(block)
                        streamed += len(block)
                        require(member_bytes <= info.file_size and streamed <= expansion_cap,
                                f"{kind} ZIP member exceeded its declared/aggregate expansion bound")
                require(member_bytes == info.file_size,
                        f"{kind} ZIP member size/CRC validation failed: {info.filename}")
        except (OSError, RuntimeError, zipfile.BadZipFile) as error:
            raise VerificationError(f"{kind} ZIP member failed bounded integrity reading") from error
        return zf, members
    except Exception:
        zf.close()
        raise


def _read_zip_json(zf: zipfile.ZipFile, info: zipfile.ZipInfo, *, limit: int = MAX_ZIP_JSON) -> Any:
    require(info.file_size <= limit, f"ZIP JSON exceeds its read bound: {info.filename}")
    try:
        with zf.open(info, "r") as source:
            raw = source.read(limit + 1)
            require(len(raw) <= limit and len(raw) == info.file_size,
                    f"ZIP JSON size mismatch: {info.filename}")
            require(source.read(1) == b"", f"ZIP JSON expands beyond its declared size: {info.filename}")
        return _parse_json(raw, info.filename)
    except (OSError, UnicodeError, json.JSONDecodeError, zipfile.BadZipFile, RuntimeError) as error:
        raise VerificationError(f"invalid ZIP JSON member: {info.filename}") from error


def _extract_native(zf: zipfile.ZipFile, members: dict[str, zipfile.ZipInfo], output_root: Path) -> Path:
    extracted = output_root / "native-extracted"
    require(not extracted.exists() and not extracted.is_symlink(),
            "refusing to reuse an existing native extraction destination")
    extracted.mkdir(mode=0o700)
    for name in sorted(NATIVE_FILES):
        destination = extracted.joinpath(*PurePosixPath(name).parts)
        destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        require(not destination.exists() and not destination.is_symlink(), f"refusing to overwrite extracted file: {name}")
        info = members[name]
        try:
            with zf.open(info, "r") as source, destination.open("xb") as target:
                remaining = info.file_size
                while remaining:
                    block = source.read(min(64 * 1024, remaining + 1))
                    require(bool(block) and len(block) <= remaining, f"native member exceeded declared size: {name}")
                    target.write(block)
                    remaining -= len(block)
                require(source.read(1) == b"", f"native member exceeds declared size: {name}")
            destination.chmod(0o600)
        except (OSError, RuntimeError, zipfile.BadZipFile) as error:
            raise VerificationError(f"cannot extract native member: {name}") from error
    return extracted


def _load_media_control(control_root: Path, manifest: dict[str, Any]) -> Any:
    path = control_root / "media-network-e2e.py"
    expected = manifest["files"]["media-network-e2e.py"]
    ui_path = control_root / "ui-e2e.py"
    ui_expected = manifest["files"]["ui-e2e.py"]
    raw, _ = _read_file(path, MAX_CONTROL_FILE, "media-network-e2e.py")
    ui_raw, _ = _read_file(ui_path, MAX_CONTROL_FILE, "ui-e2e.py")
    require(_sha256(raw) == expected, "control helper hash mismatch: media-network-e2e.py")
    require(_sha256(ui_raw) == ui_expected, "control helper hash mismatch: ui-e2e.py")
    name = "trusted_media_network_e2e_" + expected[:16]
    require(name not in sys.modules, "unexpected preloaded trusted media module")
    ui_name = "media_network_ui_e2e"
    require(ui_name not in sys.modules, "unexpected preloaded trusted UI module")

    class SnapshotLoader:
        def __init__(self, source_path: Path, source: bytes) -> None:
            self.source_path = source_path
            self.source = source

        def create_module(self, spec: Any) -> None:
            return None

        def exec_module(self, module: Any) -> None:
            module.__file__ = str(self.source_path)
            module.__package__ = ""
            exec(compile(self.source, str(self.source_path), "exec"), module.__dict__)

    original_spec_from_file_location = importlib.util.spec_from_file_location
    original_module_from_spec = importlib.util.module_from_spec

    def importlib_util_facade() -> Any:
        return types.SimpleNamespace(
            spec_from_file_location=spec_from_file_location,
            module_from_spec=original_module_from_spec,
        )

    def spec_from_file_location(module_name: str, location: Any, *args: Any, **kwargs: Any) -> Any:
        candidate = Path(location)
        if module_name == ui_name and candidate == ui_path:
            loader = SnapshotLoader(ui_path, ui_raw)
            return importlib.machinery.ModuleSpec(module_name, loader, origin=str(ui_path))
        return original_spec_from_file_location(module_name, location, *args, **kwargs)

    import builtins

    original_import = builtins.__import__
    scoped_import = dict(vars(builtins))
    scoped_import["__import__"] = lambda import_name, globals=None, locals=None, fromlist=(), level=0: (
        types.SimpleNamespace(util=importlib_util_facade())
        if import_name == "importlib.util" and not level
        else original_import(import_name, globals, locals, fromlist, level)
    )
    module = types.ModuleType(name)
    module.__file__ = str(path)
    module.__package__ = ""
    module.__dict__["__builtins__"] = scoped_import
    sys.modules[name] = module
    try:
        exec(compile(raw, str(path), "exec"), module.__dict__)
    except Exception as error:
        sys.modules.pop(name, None)
        sys.modules.pop(ui_name, None)
        raise VerificationError("pinned media helper import failed") from error
    ui = getattr(module, "ui", None)
    require(Path(getattr(module, "__file__", "")).resolve() == path,
            "media validator import escaped control bundle")
    require(ui is not None and Path(getattr(ui, "__file__", "")).resolve() == ui_path,
            "transitive UI validator import escaped control bundle")
    return module


def _summarize_native_report(report: dict[str, Any]) -> dict[str, Any]:
    probe = report.get("probe")
    probe = probe if isinstance(probe, dict) else {}
    expectations = probe.get("video_size_expectations")
    entries = expectations.get("entries") if isinstance(expectations, dict) else None
    request_block = report.get("requests")
    request_block = request_block if isinstance(request_block, dict) else {}
    recovery_request = request_block.get("first_recovery_high_media_request")
    recovery_request = recovery_request if isinstance(recovery_request, dict) else {}
    return {
        "case_full_name": report.get("case_full_name"), "status": report.get("status"),
        "diagnostic_only": report.get("diagnostic_only"), "scenario": report.get("scenario"),
        "source_duration_seconds": report.get("source_duration_seconds"),
        "decoded_frame_count": probe.get("decoded_frame_count"),
        "audio_track_present": probe.get("audio_track_present"),
        "enabled_audio_track_count": probe.get("enabled_audio_track_count"),
        "requested_duration_seconds": probe.get("requested_duration_seconds"),
        "overall_deadline_seconds": probe.get("overall_deadline_seconds"),
        "video_size_entries": [
            {key: entry.get(key) for key in
             ("expected_width", "expected_height", "matched", "required_distinct_frame_count")}
            for entry in entries if isinstance(entry, dict)
        ] if isinstance(entries, list) else [],
        "first_recovery_high_media_request": {
            "monotonic_seconds": recovery_request.get("monotonic_seconds"),
            "media_window": recovery_request.get("media_window"),
        },
    }


def _verify_native(zf: zipfile.ZipFile, members: dict[str, zipfile.ZipInfo], metadata: dict[str, Any],
                   output_root: Path, media: Any) -> dict[str, Any]:
    extracted = _extract_native(zf, members, output_root)
    try:
        media.validate_cargo_log(extracted / "logs/cargo-test.log")
    except Exception as error:
        raise VerificationError(f"native Cargo log validation failed: {error}") from error
    for filename in ("cargo-compile.log", "swift-compile.log"):
        try:
            (extracted / "logs" / filename).read_bytes().decode("utf-8")
        except (OSError, UnicodeError) as error:
            raise VerificationError(f"native compile log is missing or not UTF-8: {filename}") from error
    try:
        diagnostics = media._validate_reports_and_derive_diagnostics(extracted / "rust")
    except Exception as error:
        raise VerificationError(f"native report validation failed: {error}") from error
    summary = _read_json(extracted / "summary.json", "native summary", MAX_ZIP_JSON)
    require(isinstance(summary, dict) and type(summary.get("schema_version")) is int
            and summary.get("schema_version") == 1 and summary.get("status") == "passed",
            "native runner summary does not report passed")
    require(summary.get("recovery_latency_diagnostics") == diagnostics,
            "native summary diagnostics do not exactly match released-helper report validation")
    try:
        mute = (extracted / "logs/system-mute-verify.log").read_text(encoding="utf-8").strip()
    except (OSError, UnicodeError) as error:
        raise VerificationError("native system-mute verification log is unreadable") from error
    require(mute == "true", "native system-mute verification log is not exactly true")
    raw_metrics = {}
    for name in ("abr", "buffered", "transcoding"):
        report_path = extracted / f"rust/{name}.json"
        report = _read_json(report_path, f"native {name} report", MAX_ZIP_JSON)
        require(isinstance(report, dict), f"native report is not a JSON object: {name}")
        raw, _ = _read_file(report_path, MAX_ZIP_JSON, f"native {name} report")
        raw_metrics[name] = {"sha256": _sha256(raw), "metrics": _summarize_native_report(report)}
    return {"validation": "passed", "summary_status": summary["status"],
            "system_mute_verified": True, "raw_metrics": raw_metrics,
            "derived_diagnostics": diagnostics, "artifact": metadata}


def _verify_ui(zf: zipfile.ZipFile, members: dict[str, zipfile.ZipInfo], metadata: dict[str, Any],
               media: Any) -> dict[str, Any]:
    by_basename: dict[str, list[zipfile.ZipInfo]] = {name: [] for name in UI_JSON_NAMES}
    for info in members.values():
        basename = PurePosixPath(info.filename).name
        if basename in by_basename:
            by_basename[basename].append(info)
    require(all(len(entries) == 1 for entries in by_basename.values()),
            "UI ZIP must have exactly one occurrence of each required JSON basename")
    payloads = {name: _read_zip_json(zf, entries[0]) for name, entries in by_basename.items()}
    ui = media.ui
    platform = metadata["platform"]
    require(platform in ("macos", "tvos"), "UI artifact platform is invalid")
    config = ui.PLATFORMS[platform]
    counts = ui.validate_xcresult(payloads["xcresult-summary.json"], payloads["xcresult-tests.json"],
                                  config.test_target, ui.EXPECTED_CASES_BY_PLATFORM[platform])
    require(payloads["result-validation.json"] == counts,
            "UI result-validation receipt differs from revalidated xcresult counts")
    fixture = ui.validate_task_fixture_metrics(payloads["task-fixture-metrics.json"])
    require(set(fixture["scenarios"]) == {"automatic", "manual", "silent", "resumption"},
            "UI artifact does not prove every required fixture scenario")
    require(payloads["task-fixture-validation.json"] == fixture,
            "UI task-fixture-validation receipt differs from revalidated fixture metrics")
    require(counts == {"totalTestCount": 5, "passedTests": 5, "failedTests": 0,
                       "skippedTests": 0, "target_passed": 5, "target_failed": 0,
                       "target_skipped": 0},
            "UI artifact does not prove five named passing cases and zero failures/skips")
    target_nodes = [node for node in ui._all_dicts(payloads["xcresult-tests.json"])
                    if ui._is_expected_target(node, config.test_target)]
    target_cases = [case for target in target_nodes for case in ui._all_dicts(ui._node_children(target))
                    if "case" in str(case.get("nodeType", "")).lower()
                    or "method" in str(case.get("nodeType", "")).lower()]
    named_counts: dict[str, dict[str, int]] = {}
    for expected_case in ui.EXPECTED_CASES_BY_PLATFORM[platform]:
        matches = [case for case in target_cases
                   if re.search(rf"(?<![A-Za-z0-9_]){re.escape(expected_case)}(?![A-Za-z0-9_])",
                                str(case.get("name", "")))]
        case_counts = {"passed": 0, "failed": 0, "skipped": 0}
        for case in matches:
            for status, count in ui._test_status_counts(case).items():
                case_counts[status] += count
        require(len(matches) == 1 and case_counts == {"passed": 1, "failed": 0, "skipped": 0},
                f"UI case does not have exactly one passing result: {expected_case}")
        named_counts[expected_case] = case_counts
    return {"validation": "passed", "platform": platform,
            "xcresult_counts": counts, "named_case_counts": named_counts,
            "fixture_metrics": fixture, "artifact": metadata}


def _canonical_root(path: Path, label: str, *, exists: bool = True) -> Path:
    require(path.is_absolute(), f"{label} path must be absolute")
    if exists:
        require(path.resolve() == path and path.is_dir(), f"{label} root must be a canonical directory")
    else:
        require(not path.exists() and not path.is_symlink(), f"{label} root must be fresh")
        require(path.parent.resolve() == path.parent and path.parent.is_dir(),
                f"{label} parent must be a canonical directory")
        require(path.resolve() == path, f"{label} path must be canonical")
    return path


def _overlap(left: Path, right: Path) -> bool:
    return left == right or left in right.parents or right in left.parents


def _prepare_output(output_root: Path, roots: list[Path]) -> Path:
    output = _canonical_root(output_root, "derived output", exists=False)
    for root in roots:
        require(not _overlap(output, root), "derived output must be separate from trusted scope, subject, control, and raw input")
    output.mkdir(mode=0o700)
    return output


def _snapshot_evidence(root: Path) -> dict[str, Any]:
    snapshot = _inventory(root, readonly=False, file_limit=MAX_EVIDENCE_TREE,
                          tree_limit=MAX_EVIDENCE_TREE)
    require(snapshot["files"], "raw evidence input is empty")
    return snapshot


def _same_snapshot(before: dict[str, Any], after: dict[str, Any], label: str) -> None:
    require(before == after, f"{label} object, access policy, path set, size, or content changed during validation")


def _write_output(path: Path, payload: bytes, label: str) -> None:
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
    except FileExistsError as error:
        raise VerificationError(f"refusing to overwrite {label}") from error
    except OSError as error:
        raise VerificationError(f"cannot create {label}") from error
    identity = _identity(os.fstat(descriptor))
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
    except OSError as error:
        try:
            current = path.lstat()
            if _identity(current) == identity:
                path.unlink()
        except OSError:
            pass
        raise VerificationError(f"cannot write {label}") from error


def validate_all(*, scope_path: Path, checkout: Path, control_root: Path,
                 control_release_id: str, control_manifest_sha256: str,
                 input_root: Path, output_root: Path) -> dict[str, Any]:
    """Validate immutable reports using only the pinned control bundle."""
    scope_path = _canonical_root(scope_path.parent, "trusted scope parent") / scope_path.name
    checkout = _canonical_root(checkout, "subject checkout")
    control_root = _canonical_root(control_root, "control bundle")
    input_root = _canonical_root(input_root, "raw evidence")
    require(scope_path.is_absolute() and scope_path.resolve() == scope_path and scope_path.is_file(),
            "trusted scope file must be an existing canonical file")
    all_roots = [scope_path, checkout, control_root, input_root]
    for index, left in enumerate(all_roots):
        for right in all_roots[index + 1:]:
            require(not _overlap(left, right), "trusted scope, subject, control, and raw evidence must be separate")
    output = _prepare_output(output_root, all_roots)
    try:
        require(sys.flags.isolated == 1 and sys.flags.no_site == 1,
                "run the trusted entrypoint with python3 -I -S")
        scope, scope_before = _scope_file(scope_path)
        _scope_identity(scope)
        control_before = _check_control_bundle(control_root, control_release_id, control_manifest_sha256)
        require(Path(__file__).resolve() == control_root / "artifact_validation.py",
                "validation core is not executing from the selected control bundle")
        entrypoint = control_root / "verify_ci_artifacts.py"
        require(Path(sys.argv[0]).resolve() == entrypoint,
                "trusted control entrypoint is not the running program")
        helper_source = _validate_checkout(checkout, scope)
        raw_before = _snapshot_evidence(input_root)
        manifest = _validate_manifest(
            _read_json(_input_path(input_root, "manifest.json"), "evidence manifest"), scope, input_root,
        )
        media = _load_media_control(control_root, control_before["manifest"])
        final = _final_readback(input_root, scope)
        proof: dict[str, Any] = {
            "schema_version": SCHEMA_VERSION,
            "validation": "passed",
            "repository": scope["repository"],
            "repository_id": scope["repository_id"],
            "pr_number": scope["pr_number"],
            "base_sha": scope["base_sha"],
            "feature_head_sha": scope["feature_head_sha"],
            "feature_tree_sha": scope["feature_tree_sha"],
            "producer_source": helper_source,
            "trusted_scope": {"sha256": scope_before["sha256"], "size_bytes": scope_before["size"]},
            "validator_control": {
                "release_id": control_release_id,
                "manifest_sha256": control_manifest_sha256,
                "files": control_before["manifest"]["files"],
            },
            "raw_inputs": {
                name: {"sha256": record["sha256"], "size_bytes": record["size"]}
                for name, record in raw_before["files"].items()
            },
            "final_scope_readback": final,
            "runs": {},
            "artifacts": {},
        }
        total_raw = 0
        total_expanded = 0
        for kind in KINDS:
            run_key = "ci" if kind == "native" else kind
            run_ref = manifest["runs"][run_key]
            run_doc = _read_json(_input_path(input_root, run_ref["file"]), f"{kind} run record")
            run_sha = _validate_run(run_doc, run_ref, kind, scope, input_root)
            proof["runs"][kind] = {
                "run_id": run_ref["run_id"], "run_attempt": run_ref["run_attempt"],
                "workflow_name": run_ref["workflow_name"], "jobs_page_count": run_ref["jobs_page_count"],
                "jobs_total_count": run_ref["jobs_total_count"], "jobs": run_doc["jobs"],
            }
            artifact_ref = manifest["artifacts"][kind]
            archive, meta = _validate_artifact(artifact_ref, run_sha, scope, input_root, kind)
            total_raw += meta["size_in_bytes"]
            require(total_raw <= MAX_RAW_ARCHIVES, "aggregate ZIP download limit exceeded")
            cap = MAX_EXPANDED - total_expanded
            zf, members = _inspect_zip(archive, kind=kind, expansion_cap=cap)
            try:
                total_expanded += sum(info.file_size for info in members.values())
                require(total_expanded <= MAX_EXPANDED, "aggregate expanded ZIP limit exceeded")
                if kind == "native":
                    proof["artifacts"][kind] = _verify_native(zf, members, meta, output, media)
                else:
                    proof["artifacts"][kind] = _verify_ui(zf, members, {**meta, "platform": kind}, media)
            finally:
                zf.close()
        proof["expanded_bytes"] = total_expanded
        proof["raw_archive_bytes"] = total_raw
        encoded = json.dumps(proof, sort_keys=True, indent=2, ensure_ascii=True).encode("utf-8") + b"\n"
        require(len(encoded) <= MAX_DIAGNOSTICS, "derived diagnostics exceed 4 MiB")
        require(Path(__file__).resolve() == control_root / "artifact_validation.py",
                "validation core path changed during verification")
        _same_snapshot(scope_before, _scope_file(scope_path)[1], "trusted scope")
        _same_snapshot(control_before["snapshot"],
                       _check_control_bundle(control_root, control_release_id, control_manifest_sha256)["snapshot"],
                       "control bundle")
        _same_snapshot(raw_before, _snapshot_evidence(input_root), "raw evidence")
        require(_validate_checkout(checkout, scope) == helper_source,
                "subject checkout or producer helper identity changed during validation")
        _write_output(output / "proof.json", encoded, "proof output")
        return proof
    except Exception as error:
        failure = {
            "schema_version": SCHEMA_VERSION,
            "validation": "failed",
            "error_type": type(error).__name__,
            "error": str(error)[:1000],
        }
        try:
            if not (output / "proof.json").exists():
                _write_output(output / "failure.json",
                              json.dumps(failure, sort_keys=True, indent=2).encode("utf-8") + b"\n",
                              "failure record")
        except Exception:
            pass
        if isinstance(error, VerificationError):
            raise
        raise VerificationError(f"artifact validation failed: {error}") from error

