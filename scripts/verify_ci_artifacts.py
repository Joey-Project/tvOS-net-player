#!/usr/bin/env python3
"""Validate parent-acquired CI artifacts with a separately pinned control bundle."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import sys
import types
from pathlib import Path
from typing import Any


CONTROL_MANIFEST = "control-manifest.json"
CONTROL_FILES = {
    "verify_ci_artifacts.py", "artifact_validation.py", "media-network-e2e.py", "ui-e2e.py",
}
MAX_CONTROL_FILE = 8 * 1024 * 1024
MAX_CONTROL_TREE = 32 * 1024 * 1024


class BootstrapError(RuntimeError):
    pass


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise BootstrapError(message)


def _pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        _require(key not in result, f"duplicate control manifest key: {key}")
        result[key] = value
    return result


def _read(path: Path, label: str, limit: int) -> bytes:
    try:
        before = path.lstat()
    except FileNotFoundError as error:
        raise BootstrapError(f"missing control file: {label}") from error
    except OSError as error:
        raise BootstrapError(f"unreadable control file metadata: {label}") from error
    _require(stat.S_ISREG(before.st_mode) and before.st_nlink == 1,
             f"unsafe control file: {label}")
    _require(stat.S_IMODE(before.st_mode) & 0o222 == 0, f"control file is writable: {label}")
    _require(before.st_size <= limit, f"control file exceeds byte limit: {label}")
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    except FileNotFoundError as error:
        raise BootstrapError(f"missing control file: {label}") from error
    except OSError as error:
        raise BootstrapError(f"unreadable control file: {label}") from error
    try:
        opened = os.fstat(descriptor)
        _require((opened.st_dev, opened.st_ino, stat.S_IMODE(opened.st_mode), opened.st_nlink)
                 == (before.st_dev, before.st_ino, stat.S_IMODE(before.st_mode), before.st_nlink),
                 f"control file object changed while opening: {label}")
        chunks: list[bytes] = []
        total = 0
        while True:
            try:
                block = os.read(descriptor, min(1024 * 1024, limit + 1 - total))
            except OSError as error:
                raise BootstrapError(f"unreadable control file: {label}") from error
            if not block:
                break
            total += len(block)
            _require(total <= limit, f"control file exceeds byte limit: {label}")
            chunks.append(block)
        after = os.fstat(descriptor)
        _require((after.st_dev, after.st_ino, stat.S_IMODE(after.st_mode), after.st_nlink)
                 == (opened.st_dev, opened.st_ino, stat.S_IMODE(opened.st_mode), opened.st_nlink)
                 and total == opened.st_size, f"control file changed while reading: {label}")
        return b"".join(chunks)
    finally:
        os.close(descriptor)


def _check_bundle(root: Path, release_id: str, manifest_digest: str) -> tuple[dict[str, Any], bytes]:
    _require(root.is_absolute() and root.resolve() == root and root.is_dir(),
             "control root must be an existing canonical directory")
    root_info = root.lstat()
    _require(stat.S_IMODE(root_info.st_mode) & 0o222 == 0, "control root is writable")
    seen: set[str] = set()
    total = 0
    verified_files: dict[str, bytes] = {}
    try:
        entries = sorted(os.scandir(root), key=lambda entry: entry.name)
    except OSError as error:
        raise BootstrapError("control root is unreadable") from error
    for entry in entries:
        name = entry.name
        try:
            info = entry.stat(follow_symlinks=False)
        except OSError as error:
            raise BootstrapError(f"unreadable control entry: {name}") from error
        _require(stat.S_ISREG(info.st_mode), f"unexpected control directory or special entry: {name}")
        _require(name in CONTROL_FILES | {CONTROL_MANIFEST}, f"undeclared control file: {name}")
        raw = _read(root / name, name, MAX_CONTROL_FILE if name != CONTROL_MANIFEST else 1024 * 1024)
        verified_files[name] = raw
        total += len(raw)
        _require(total <= MAX_CONTROL_TREE, "control bundle exceeds aggregate byte limit")
        seen.add(name)
    _require(seen == CONTROL_FILES | {CONTROL_MANIFEST}, "control bundle has missing files")
    manifest_raw = _read(root / CONTROL_MANIFEST, CONTROL_MANIFEST, 1024 * 1024)
    _require(hashlib.sha256(manifest_raw).hexdigest() == manifest_digest,
             "control manifest digest mismatch")
    try:
        manifest = json.loads(manifest_raw.decode("utf-8"), object_pairs_hook=_pairs)
    except BootstrapError:
        raise
    except (UnicodeError, json.JSONDecodeError) as error:
        raise BootstrapError("control manifest is invalid JSON") from error
    _require(isinstance(manifest, dict) and set(manifest) == {"schema_version", "release_id", "files"},
             "control manifest fields/schema mismatch")
    _require(type(manifest["schema_version"]) is int and manifest["schema_version"] == 1,
             "unsupported control manifest schema")
    _require(manifest["release_id"] == release_id, "control release identity mismatch")
    _require(isinstance(manifest["files"], dict) and set(manifest["files"]) == CONTROL_FILES,
             "control manifest file list mismatch")
    for name in CONTROL_FILES:
        raw = verified_files[name]
        digest = manifest["files"][name]
        _require(isinstance(digest, str) and len(digest) == 64
                 and all(character in "0123456789abcdef" for character in digest)
                 and hashlib.sha256(raw).hexdigest() == digest,
                 f"control file hash mismatch: {name}")
    return manifest, verified_files["artifact_validation.py"]


def _load_validation_core(root: Path, raw: bytes) -> types.ModuleType:
    path = root / "artifact_validation.py"
    module = types.ModuleType("pinned_artifact_validation")
    module.__file__ = str(path)
    module.__package__ = ""
    sys.modules[module.__name__] = module
    try:
        exec(compile(raw, str(path), "exec"), module.__dict__)
    except Exception as error:
        sys.modules.pop(module.__name__, None)
        raise BootstrapError("pinned validation core could not be loaded") from error
    return module


def _record_failure(args: argparse.Namespace, error: Exception) -> None:
    try:
        root = args.control_root
        entrypoint = root / "verify_ci_artifacts.py"
        if not root.is_absolute() or root.resolve() != root or Path(__file__).resolve() != entrypoint:
            return
        trusted_paths = (args.scope, args.checkout, root, args.input_root)
        if any(not path.is_absolute() or path.resolve() != path for path in trusted_paths):
            return
        if any(not path.exists() for path in trusted_paths):
            return
        output = args.output_root
        if (not output.is_absolute() or output.exists() or output.is_symlink()
                or output.parent.resolve() != output.parent or not output.parent.is_dir()):
            return
        for path in trusted_paths:
            if output == path or output in path.parents or path in output.parents:
                return
        output.mkdir(mode=0o700)
        payload = json.dumps({
            "schema_version": 1, "validation": "failed",
            "error_type": type(error).__name__, "error": str(error)[:1000],
        }, sort_keys=True, indent=2).encode("utf-8") + b"\n"
        record = output / "failure.json"
        descriptor = os.open(record, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
    except Exception:
        return


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scope", type=Path, required=True, help="absolute operator-assigned trusted scope JSON")
    parser.add_argument("--checkout", type=Path, required=True, help="absolute assigned subject checkout")
    parser.add_argument("--control-root", type=Path, required=True, help="read-only pinned control bundle root")
    parser.add_argument("--control-release", required=True, help="operator-pinned control release identity")
    parser.add_argument("--control-manifest-sha256", required=True,
                        help="operator-pinned SHA-256 of control-manifest.json")
    parser.add_argument("--input-root", type=Path, required=True, help="immutable raw evidence root")
    parser.add_argument("--output-root", type=Path, required=True, help="fresh derived output root")
    args = parser.parse_args()
    try:
        _require(sys.flags.isolated == 1 and sys.flags.no_site == 1,
                 "run this entrypoint with python3 -I -S")
        root = args.control_root
        expected_entrypoint = root / "verify_ci_artifacts.py"
        _require(Path(__file__).resolve() == expected_entrypoint and expected_entrypoint.resolve() == expected_entrypoint,
                 "running entrypoint is not the selected control bundle entrypoint")
        manifest, core_raw = _check_bundle(root, args.control_release, args.control_manifest_sha256)
        core = _load_validation_core(root, core_raw)
        result = core.validate_all(
            scope_path=args.scope, checkout=args.checkout, control_root=root,
            control_release_id=args.control_release,
            control_manifest_sha256=args.control_manifest_sha256,
            input_root=args.input_root, output_root=args.output_root,
        )
    except Exception as error:
        _record_failure(args, error)
        print(f"artifact validation failed: {error}", file=sys.stderr)
        return 1
    kinds = ", ".join(sorted(result["artifacts"]))
    print(f"Validated exact-scope CI artifacts ({kinds}); proof: {args.output_root / 'proof.json'}")
    print("This verifies reports only; it does not establish local signed or physical playback acceptance.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
