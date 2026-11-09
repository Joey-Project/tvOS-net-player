#!/usr/bin/env python3
"""Build and verify stable, locally signed macOS products."""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import hashlib
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Callable, Sequence


ROOT = Path(__file__).resolve().parent.parent
DEFAULT_ROOT = Path.home() / "Library/Application Support/TVOSNetPlayer/LocalBuild"
DEFAULT_CONFIG = DEFAULT_ROOT / "signing.json"
IDENTIFIERS = {
    "app": "dev.joey.tvos-net-player.macos",
    "server": "dev.joey.tvos-net-player.cache-server",
    "probe": "dev.joey.tvos-net-player.playback-probe",
    "media-tests": "dev.joey.tvos-net-player.media-acceptance-tests",
}
PRODUCTS = {
    "app": Path("Apps/MacOSNetPlayer.app"),
    "server": Path("bin/tvos-net-player-cache-server"),
    "probe": Path("bin/probe-lan-playback"),
    "media-tests": Path("bin/media-acceptance-tests"),
}
RUN: Callable[..., subprocess.CompletedProcess[str]] = subprocess.run


class BuildError(RuntimeError):
    pass


def _run(args: Sequence[str], *, check: bool = True, capture: bool = True) -> str:
    try:
        result = RUN(
            list(args), check=False, text=True,
            stdout=subprocess.PIPE if capture else sys.stderr,
            stderr=subprocess.PIPE if capture else sys.stderr,
        )
    except OSError as error:
        raise BuildError(f"could not run {args[0]}: {error}") from error
    if check and result.returncode:
        detail = (result.stderr or result.stdout or "").strip()
        raise BuildError(f"command failed ({result.returncode}): {args[0]}: {detail}")
    return result.stdout or ""


def config_path() -> Path:
    return Path(os.environ.get("TVOS_NET_PLAYER_LOCAL_BUILD_CONFIG", DEFAULT_CONFIG)).expanduser()


def load_config(path: Path | None = None) -> dict:
    path = path or config_path()
    try:
        config = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise BuildError(f"local signing config unavailable or invalid at {path}: {error}") from error
    required = {"schema_version", "team_id", "identity_sha1", "certificate_sha1", "expires_at"}
    if not isinstance(config, dict) or not required.issubset(config):
        raise BuildError(f"local signing config must define {', '.join(sorted(required))}")
    if config["schema_version"] != 1:
        raise BuildError("unsupported local signing config schema_version")
    for key in ("team_id", "identity_sha1", "certificate_sha1"):
        value = config[key]
        pattern = r"[A-Za-z0-9]{10}" if key == "team_id" else r"[A-Fa-f0-9]{40}"
        if not isinstance(value, str) or not re.fullmatch(pattern, value):
            raise BuildError(f"invalid {key} in local signing config")
        config[key] = value.upper()
    if config["identity_sha1"] != config["certificate_sha1"]:
        raise BuildError("identity_sha1 must match the pinned leaf certificate_sha1")
    try:
        expiry = dt.date.fromisoformat(config["expires_at"])
    except (TypeError, ValueError) as error:
        raise BuildError("expires_at must be an ISO date") from error
    if expiry < dt.date.today():
        raise BuildError("configured signing certificate has expired")
    root_value = os.environ.get("TVOS_NET_PLAYER_LOCAL_BUILD_ROOT", config.get("durable_root"))
    root = Path(root_value).expanduser() if root_value else DEFAULT_ROOT
    if not root.is_absolute():
        raise BuildError("durable_root must be an absolute path")
    config["durable_root"] = root.resolve()
    return config


def verify_identity(config: dict) -> None:
    identities = _run(["security", "find-identity", "-v", "-p", "codesigning"])
    wanted = config["identity_sha1"]
    if not re.search(rf"^\s*\d+\)\s+{wanted}\s+", identities, re.IGNORECASE | re.MULTILINE):
        raise BuildError("configured valid signing identity fingerprint is missing from the keychain")


def _metadata_output(result: subprocess.CompletedProcess[str], path: Path) -> str:
    if result.returncode:
        detail = "\n".join(part.strip() for part in (result.stderr, result.stdout) if part and part.strip())
        raise BuildError(f"could not read signed metadata from {path}: {detail or f'exit {result.returncode}'}")
    return "\n".join(part for part in (result.stdout, result.stderr) if part)


def _locked(root: Path):
    lock = root / ".build-session.lock"
    try:
        lock.mkdir()
    except FileExistsError as error:
        raise BuildError(f"another local build session holds {lock}; remove it only after confirming no build is active") from error
    return lock


def _receipt_path(root: Path) -> Path:
    return root / "receipt.json"


def _read_receipt(root: Path, *, allow_missing: bool = False) -> dict:
    path = _receipt_path(root)
    try:
        receipt = json.loads(path.read_text())
    except FileNotFoundError as error:
        if allow_missing:
            return {"schema_version": 1, "products": {}}
        raise BuildError(f"local build receipt is missing: {path}") from error
    except OSError as error:
        raise BuildError(f"could not read local build receipt at {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise BuildError(f"local build receipt is malformed at {path}: {error}") from error
    if (not isinstance(receipt, dict) or type(receipt.get("schema_version")) is not int
            or receipt.get("schema_version") != 1
            or not isinstance(receipt.get("products"), dict) or not receipt["products"]):
        raise BuildError(f"local build receipt has an invalid or empty schema: {path}")
    for target, item in receipt["products"].items():
        if (target not in PRODUCTS or not isinstance(item, dict)
                or not isinstance(item.get("path"), str)
                or not isinstance(item.get("source"), dict)
                or not isinstance(item.get("cdhash"), str) or not item["cdhash"]):
            raise BuildError(f"local build receipt has an invalid product entry for {target!r}: {path}")
    return receipt


def _validate_app_environment(root: Path) -> None:
    expected_paths = {
        "PROJECT_PATH": ROOT / "TVOSNetPlayer.xcodeproj",
        "DERIVED_DATA_PATH": root / "DerivedData" / hashlib.sha256(str(ROOT).encode()).hexdigest()[:16],
    }
    for name, expected in expected_paths.items():
        value = os.environ.get(name, "")
        if value and Path(value).expanduser().resolve() != expected.resolve():
            raise BuildError(f"{name} override is unsupported by the pinned local build; use LOCAL_BUILD_MODE=isolated with the existing custom-build wrapper")
    scheme = os.environ.get("MACOS_SCHEME", "")
    if scheme and scheme != "MacOSNetPlayer":
        raise BuildError("MACOS_SCHEME override is unsupported by the pinned local build; use LOCAL_BUILD_MODE=isolated with the existing custom-build wrapper")


def _write_receipt(root: Path, receipt: dict) -> None:
    temporary = _receipt_path(root).with_suffix(".json.tmp")
    temporary.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    temporary.replace(_receipt_path(root))


def _codesign_details(path: Path) -> tuple[str, str, str, str, str]:
    _run(["codesign", "--verify", "--deep", "--strict", "--verbose=2", str(path)])
    result = RUN(["codesign", "-dv", "--verbose=4", str(path)], check=False, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    details = _metadata_output(result, path)
    identifier = re.search(r"^Identifier=(.+)$", details, re.MULTILINE)
    team = re.search(r"^TeamIdentifier=(.+)$", details, re.MULTILINE)
    cdhash = re.search(r"^CDHash=(\S+)$", details, re.MULTILINE)
    req_result = RUN(["codesign", "-d", "-r-", str(path)], check=False, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    requirement_output = _metadata_output(req_result, path)
    requirement_match = re.search(r"^designated =>\s*(.+)$", requirement_output, re.MULTILINE)
    requirement = requirement_match.group(1).strip() if requirement_match else ""
    if not identifier or not team or not requirement:
        raise BuildError(f"could not read signed identity and designated requirement from {path}")
    if not cdhash:
        raise BuildError(f"could not read code directory hash from {path}")
    with tempfile.TemporaryDirectory(prefix="tvos-local-cert-") as temporary:
        prefix = str(Path(temporary) / "cert")
        _run(["codesign", "-d", f"--extract-certificates={prefix}", str(path)])
        leaf = Path(prefix + "0")
        fingerprint = _run(["/usr/bin/openssl", "x509", "-inform", "DER", "-in", str(leaf), "-noout", "-fingerprint", "-sha1"])
        subject = _run(["/usr/bin/openssl", "x509", "-inform", "DER", "-in", str(leaf), "-noout", "-subject", "-nameopt", "RFC2253"])
    match = re.search(r"=([A-Fa-f0-9:]{59})", fingerprint)
    organization = re.search(r"(?:^|,)OU=([^,]+)", subject.strip().removeprefix("subject="))
    if not match or not organization:
        raise BuildError(f"could not read leaf certificate fingerprint and team OU from {path}")
    if organization.group(1) != team.group(1):
        raise BuildError(f"signed TeamIdentifier does not match leaf certificate subject OU for {path}")
    return (
        identifier.group(1), team.group(1), match.group(1).replace(":", "").upper(),
        requirement, cdhash.group(1),
    )


def _expected_identifier(target: str, path: Path) -> str:
    if target == "app":
        with (path / "Contents/Info.plist").open("rb") as stream:
            actual = plistlib.load(stream).get("CFBundleIdentifier")
        if actual != IDENTIFIERS[target]:
            raise BuildError(f"app bundle identifier mismatch: expected {IDENTIFIERS[target]}, got {actual}")
    return IDENTIFIERS[target]


def verify_product(target: str, path: Path, config: dict, previous: dict | None = None) -> dict:
    identifier, team, cert_sha1, requirement, cdhash = _codesign_details(path)
    expected_id = _expected_identifier(target, path)
    if identifier != expected_id or team != config["team_id"]:
        raise BuildError(f"signed identifier/team mismatch for {path}")
    if cert_sha1 != config["certificate_sha1"]:
        raise BuildError(f"leaf certificate fingerprint mismatch for {path}")
    if previous and previous.get("designated_requirement") != requirement:
        raise BuildError(f"designated requirement changed unexpectedly for {target}")
    return {
        "path": str(path), "identifier": identifier, "team_id": team,
        "certificate_sha1": cert_sha1, "designated_requirement": requirement,
        "cdhash": cdhash,
    }


def _source_binding() -> dict:
    try:
        head = _run(["git", "-C", str(ROOT), "rev-parse", "HEAD"]).strip()
        dirty = bool(_run(["git", "-C", str(ROOT), "status", "--porcelain"]).strip())
    except BuildError:
        head = "unknown"
        dirty = True
    return {"repository": str(ROOT), "head": head, "dirty": dirty}


def _build(target: str, config: dict, root: Path, old_receipt: dict) -> dict:
    destination = root / PRODUCTS[target]
    destination.parent.mkdir(parents=True, exist_ok=True)
    previous = old_receipt.get("products", {}).get(target)
    if destination.exists() and not previous:
        raise BuildError(f"refusing to overwrite untracked existing product: {destination}")
    if destination.exists():
        if Path(previous.get("path", "")) != destination:
            raise BuildError(f"refusing to overwrite a product not bound to this stable path: {destination}")
        prior = verify_product(target, destination, config, previous)
        if prior["cdhash"] != previous.get("cdhash"):
            raise BuildError(f"refusing to overwrite a product whose CDHash differs from its receipt: {destination}")
    staging = Path(tempfile.mkdtemp(prefix=f".{destination.name}.stage-", dir=destination.parent)) / destination.name
    try:
        if target == "app":
            derived = root / "DerivedData" / hashlib.sha256(str(ROOT).encode()).hexdigest()[:16]
            _run([
                "xcodebuild", "-onlyUsePackageVersionsFromResolvedFile", "-skipPackagePluginValidation",
                "-project", str(ROOT / "TVOSNetPlayer.xcodeproj"), "-scheme", "MacOSNetPlayer",
                "-configuration", os.environ.get("CONFIGURATION", "Debug"),
                "-destination", os.environ.get("MACOS_DESTINATION", "generic/platform=macOS"),
                "-derivedDataPath", str(derived), "CODE_SIGNING_ALLOWED=YES", "CODE_SIGNING_REQUIRED=YES",
                "CODE_SIGN_STYLE=Manual", f"DEVELOPMENT_TEAM={config['team_id']}",
                f"CODE_SIGN_IDENTITY={config['identity_sha1']}",
                f"PRODUCT_BUNDLE_IDENTIFIER={IDENTIFIERS[target]}", "build",
            ], capture=False)
            built = derived / "Build/Products" / os.environ.get("CONFIGURATION", "Debug") / "MacOSNetPlayer.app"
            shutil.copytree(built, staging, symlinks=True)
        elif target == "server":
            cargo_target = root / "CargoTarget" / hashlib.sha256(str(ROOT).encode()).hexdigest()[:16]
            _run(["cargo", "build", "--manifest-path", str(ROOT / "Cargo.toml"), "--package", "tvos-net-player-cache-server", "--release", "--locked", "--target-dir", str(cargo_target)], capture=False)
            shutil.copy2(cargo_target / "release/tvos-net-player-cache-server", staging)
        elif target == "probe":
            _run([
                "xcrun", "swiftc", "-swift-version", "6", "-strict-concurrency=complete",
                "-warnings-as-errors", "-parse-as-library", str(ROOT / "scripts/probe-lan-playback.swift"),
                "-o", str(staging),
            ], capture=False)
        elif target == "media-tests":
            cargo_target = root / "CargoTarget" / hashlib.sha256(str(ROOT).encode()).hexdigest()[:16]
            output = _run([
                "cargo", "test", "--manifest-path", str(ROOT / "Cargo.toml"), "--package",
                "tvos-net-player-cache-server", "--release", "--lib", "--no-run", "--locked", "--target-dir", str(cargo_target),
                "--message-format=json",
            ], capture=True)
            artifacts = [json.loads(line) for line in output.splitlines() if line.startswith("{")]
            candidates = [Path(item["executable"]) for item in artifacts
                          if item.get("reason") == "compiler-artifact" and item.get("profile", {}).get("test")
                          and item.get("target", {}).get("name") == "tvos_net_player_cache_server"
                          and item.get("executable")]
            if len(candidates) != 1:
                raise BuildError(f"expected exactly one Rust library test executable; found {len(candidates)}")
            shutil.copy2(candidates[0], staging)
        else:
            raise BuildError(f"unsupported target {target}")

        if target != "app":
            _run(["codesign", "--force", "--sign", config["identity_sha1"], "--identifier", IDENTIFIERS[target], str(staging)])
        details = verify_product(target, staging, config, previous)
        details["path"] = str(destination)
        details["source"] = _source_binding()
        if destination.exists():
            displaced = destination.with_name(destination.name + ".previous")
            if displaced.exists():
                raise BuildError(f"refusing to overwrite existing recovery path: {displaced}")
            destination.rename(displaced)
            try:
                staging.rename(destination)
            except BaseException:
                displaced.rename(destination)
                raise
            shutil.rmtree(displaced) if displaced.is_dir() else displaced.unlink()
        else:
            staging.rename(destination)
        return details
    finally:
        if staging.exists():
            shutil.rmtree(staging) if staging.is_dir() else staging.unlink()
        parent = staging.parent
        if parent.exists() and not any(parent.iterdir()):
            parent.rmdir()


@contextlib.contextmanager
def prepared_targets(targets: set[str], *, config_file: Path | None = None):
    unknown = targets - PRODUCTS.keys()
    if unknown or not targets:
        raise BuildError(f"invalid target selection: {', '.join(sorted(unknown or targets))}")
    config = load_config(config_file)
    root: Path = config["durable_root"]
    if "app" in targets:
        _validate_app_environment(root)
    verify_identity(config)
    root.mkdir(parents=True, exist_ok=True)
    lock = _locked(root)
    try:
        receipt_path = _receipt_path(root)
        if receipt_path.exists():
            receipt = _read_receipt(root)
        else:
            orphaned = [str(root / product) for product in PRODUCTS.values() if (root / product).exists()]
            if orphaned:
                raise BuildError(f"local build receipt is missing for existing stable product: {', '.join(orphaned)}")
            receipt = _read_receipt(root, allow_missing=True)
        products = dict(receipt.get("products", {}))
        for target in sorted(targets):
            products[target] = _build(target, config, root, receipt)
            receipt = {
                "schema_version": 1, "source": _source_binding(), "team_id": config["team_id"],
                "identity_sha1": config["identity_sha1"], "certificate_sha1": config["certificate_sha1"],
                "products": products, "receipt_path": str(_receipt_path(root)),
            }
            _write_receipt(root, receipt)
        yield receipt
    finally:
        lock.rmdir()


def prepare_targets(targets: set[str], *, config_file: Path | None = None) -> dict:
    with prepared_targets(targets, config_file=config_file) as receipt:
        return receipt


def status() -> dict:
    config = load_config()
    verify_identity(config)
    root = config["durable_root"]
    root.mkdir(parents=True, exist_ok=True)
    lock = _locked(root)
    try:
        receipt = _read_receipt(root)
        if not receipt["products"]:
            raise BuildError(f"local build receipt contains no products: {_receipt_path(root)}")
        source = _source_binding()
        for target, product in PRODUCTS.items():
            if (root / product).exists() and target not in receipt["products"]:
                raise BuildError(f"stable product has no receipt entry for {target}: {root / product}")
        for target, item in receipt.get("products", {}).items():
            path = Path(item["path"])
            if target not in PRODUCTS or path != root / PRODUCTS[target]:
                raise BuildError(f"receipt contains an unexpected product path: {path}")
            if item.get("source") != source:
                raise BuildError(f"receipt source state does not match the current checkout for {target}")
            actual = verify_product(target, path, config, item)
            if actual["cdhash"] != item.get("cdhash"):
                raise BuildError(f"signed product CDHash differs from its receipt for {target}")
        return receipt
    finally:
        lock.rmdir()


def isolated() -> bool:
    return os.environ.get("LOCAL_BUILD_MODE", "").lower() == "isolated" or os.environ.get("CI", "").lower() == "true"


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("status")
    build = sub.add_parser("build")
    build.add_argument("targets", nargs="+", choices=sorted(PRODUCTS))
    prepare = sub.add_parser("prepare")
    prepare.add_argument("targets", nargs="+", choices=sorted(PRODUCTS))
    prepare.add_argument("--json", action="store_true")
    prepare.add_argument("--print-path", choices=sorted(PRODUCTS))
    run = sub.add_parser("run")
    run.add_argument("target", choices=("probe", "media-tests"))
    run.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    try:
        if args.command == "run":
            arguments = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
            with prepared_targets({args.target}) as receipt:
                executable = receipt["products"][args.target]["path"]
                return subprocess.run([executable, *arguments], check=False).returncode
        result = status() if args.command == "status" else prepare_targets(set(args.targets))
        if args.command == "prepare" and args.print_path:
            print(result["products"][args.print_path]["path"])
            return 0
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0
    except BuildError as error:
        print(f"local build: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
