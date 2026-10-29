"""Validate frozen Rust release attachments locally or against an existing Release.

This module uses only the Python standard library. Remote verification performs
GET/download operations only; it never creates a Release or uploads an asset.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import stat
import subprocess
import sys
from urllib.parse import quote
import urllib.request
import zipfile


REPOSITORY = "ceastld/eaclip"
TARGETS = {"x64": "x86_64-pc-windows-msvc", "arm64": "aarch64-pc-windows-msvc"}
SCHEMA = "cliphost.rust-release-assets.v1"


class AssetValidationError(RuntimeError):
    pass


def _version(value):
    if not isinstance(value, str) or not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise AssetValidationError("Expected a three-part Rust release version")
    return value


def _architectures(values):
    values = tuple(values)
    if not values or len(values) != len(set(values)) or any(value not in TARGETS for value in values):
        raise AssetValidationError("Select x64 and/or arm64 once each")
    return values


def _regular(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
        raise AssetValidationError(f"Release assets must be ordinary files: {path.name}")
    return info


def _sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8-sig"))
    except (ValueError, UnicodeError) as error:
        raise AssetValidationError(f"Invalid JSON: {path.name}") from error


def _reject_archive_symbols(path):
    try:
        with zipfile.ZipFile(path) as archive:
            if any(Path(entry.filename).suffix.lower() == ".pdb" for entry in archive.infolist()):
                raise AssetValidationError(f"Public release ZIP cannot contain PDB files: {path.name}")
    except zipfile.BadZipFile as error:
        raise AssetValidationError(f"Invalid release ZIP: {path.name}") from error


def _write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def validate_local_assets(version, asset_directory, architectures=("x64", "arm64")):
    """Return a manifest after checking the exact three frozen files per architecture."""
    version = _version(version)
    architectures = _architectures(architectures)
    original = Path(asset_directory)
    if original.is_symlink() or getattr(original.stat(), "st_file_attributes", 0) & 0x400:
        raise AssetValidationError("The asset directory cannot be a link or junction")
    directory = original.resolve()
    expected_names = set()
    for arch in architectures:
        stem = f"eaclip-{version}-windows-{arch}"
        expected_names.update((f"{stem}.zip", f"{stem}.build-info.json", f"{stem}.SHA256SUMS.txt"))
    actual_names = {path.name for path in directory.iterdir()}
    if actual_names != expected_names:
        raise AssetValidationError("Expected exactly the selected architecture-specific release assets; "
                                   f"missing={sorted(expected_names - actual_names)}, "
                                   f"unexpected={sorted(actual_names - expected_names)}")
    for name in sorted(expected_names):
        _regular(directory / name)
    files = []
    for arch in architectures:
        stem = f"eaclip-{version}-windows-{arch}"
        info_name = f"{stem}.build-info.json"
        payload_names = {f"{stem}.zip", info_name}
        checksum_name = f"{stem}.SHA256SUMS.txt"
        info = _read_json(directory / info_name)
        expected = {"product": "ClipHost Rust", "version": f"{version}.0", "architecture": arch,
                    "target": TARGETS[arch], "renderer": "gpui", "profile": "release"}
        if not isinstance(info, dict):
            raise AssetValidationError(f"Expected a build-info object: {info_name}")
        for key, value in expected.items():
            if info.get(key) != value:
                raise AssetValidationError(f"{info_name}: {key} does not match {value!r}")
        if info.get("embeddedFrontend") is not False:
            raise AssetValidationError(f"{info_name}: embeddedFrontend must be false")
        checksums = {}
        for line in (directory / checksum_name).read_text(encoding="utf-8-sig").splitlines():
            match = re.fullmatch(r"([0-9a-fA-F]{64})  (.+)", line)
            if not match or match[2] not in payload_names or match[2] in checksums:
                raise AssetValidationError(f"Invalid or duplicate checksum entry: {checksum_name}")
            checksums[match[2]] = match[1].lower()
        if checksums.keys() != payload_names:
            raise AssetValidationError(f"Incomplete checksums: {checksum_name}")
        for name in sorted(payload_names | {checksum_name}):
            path = directory / name
            digest = _sha256(path)
            if name in checksums and checksums[name] != digest:
                raise AssetValidationError(f"Checksum mismatch: {name}")
            if name.endswith(".zip"):
                _reject_archive_symbols(path)
            files.append({"name": name, "path": str(path), "architecture": arch,
                          "sizeBytes": path.stat().st_size, "sha256": digest})
    return {"schema": SCHEMA, "version": version, "tag": f"rust-v{version}",
            "architectures": list(architectures), "assetDirectory": str(directory),
            "files": sorted(files, key=lambda value: value["name"])}


def validate_remote_release(release, manifest, *, expected_draft=False, repository=REPOSITORY):
    """Check Release metadata against a freshly validated local manifest; no I/O."""
    if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
        raise AssetValidationError("Invalid GitHub repository")
    if not isinstance(release, dict) or release.get("tag_name") != manifest["tag"]:
        raise AssetValidationError("Published release tag does not match the source version")
    if release.get("draft") is not expected_draft:
        raise AssetValidationError("Published release draft state does not match the request")
    assets = release.get("assets")
    if not isinstance(assets, list) or any(not isinstance(asset, dict) for asset in assets):
        raise AssetValidationError("Invalid remote release asset list")
    remote = {}
    for asset in assets:
        name = asset.get("name")
        if not isinstance(name, str) or name in remote:
            raise AssetValidationError("Invalid or duplicate remote release asset name")
        remote[name] = asset
    local = {file["name"]: file for file in manifest["files"]}
    if remote.keys() != local.keys():
        raise AssetValidationError("Uploaded release assets do not match the prepared assets")
    checked = []
    for name, file in sorted(local.items()):
        asset = remote[name]
        if asset.get("state") != "uploaded" or type(asset.get("size")) is not int or asset["size"] != file["sizeBytes"]:
            raise AssetValidationError(f"Uploaded size/state mismatch: {name}")
        if asset.get("digest") != f"sha256:{file['sha256']}":
            raise AssetValidationError(f"Uploaded server SHA-256 mismatch: {name}")
        url = f"https://github.com/{repository}/releases/download/{quote(manifest['tag'], safe='')}/{quote(name, safe='')}"
        if asset.get("browser_download_url") != url:
            raise AssetValidationError(f"Unexpected public download URL: {name}")
        checked.append({"name": name, "sizeBytes": file["sizeBytes"], "sha256": file["sha256"],
                        "downloadUrl": url})
    return checked


def _new_output_directory(value):
    output = Path(value)
    if output.is_symlink() or (output.exists() and getattr(output.stat(), "st_file_attributes", 0) & 0x400):
        raise AssetValidationError("The verification output cannot be a link or junction")
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        raise AssetValidationError("Verification output must be new or empty; previous receipts are never overwritten")
    return output


def _run_gh(arguments, gh_executable):
    result = subprocess.run([gh_executable, *arguments], capture_output=True, text=True,
                            encoding="utf-8", timeout=180, check=False)
    if result.returncode:
        raise AssetValidationError(f"GitHub read/download command failed (exit {result.returncode})")
    return result.stdout


def _download_public_asset(asset, destination):
    # urllib has no access to the CLI's token; this request deliberately supplies
    # no Authorization header, including when GH_TOKEN is set for metadata GETs.
    request = urllib.request.Request(asset["downloadUrl"], headers={"User-Agent": "ClipHost-Release-Verification"})
    received = 0
    with urllib.request.urlopen(request, timeout=120) as response, destination.open("xb") as stream:
        while chunk := response.read(1024 * 1024):
            received += len(chunk)
            if received > asset["sizeBytes"]:
                raise AssetValidationError(f"Downloaded size mismatch: {asset['name']}")
            stream.write(chunk)


def verify_remote_release(version, asset_directory, *, repository=REPOSITORY, expected_draft=False,
                          architectures=("x64", "arm64"), output_directory, gh_executable="gh"):
    """Read and download an existing Release, retaining a receipt even on failure.

    A new output directory is mandatory on every run. Public assets use anonymous
    HTTPS; drafts use only ``gh release download`` with the caller's existing login.
    """
    output = _new_output_directory(output_directory)
    receipt = {"schema": SCHEMA, "status": "verifying", "mode": "remote", "version": version,
               "repository": repository, "expectedDraft": expected_draft, "readOnly": True,
               "startedAtUtc": datetime.now(timezone.utc).isoformat(), "assets": []}
    try:
        manifest = validate_local_assets(version, asset_directory, architectures)
        _write_json(output / "local-manifest.json", manifest)
        if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
            raise AssetValidationError("Invalid GitHub repository")
        raw = _run_gh(["api", f"repos/{repository}/releases/tags/{manifest['tag']}"], gh_executable)
        try:
            release = json.loads(raw)
        except ValueError as error:
            raise AssetValidationError("GitHub returned invalid Release JSON") from error
        _write_json(output / "release.json", release)
        checked = validate_remote_release(release, manifest, expected_draft=expected_draft, repository=repository)
        receipt.update(tag=manifest["tag"], url=release.get("html_url"), draft=release["draft"])
        downloaded = output / "downloaded"
        downloaded.mkdir()
        if expected_draft:
            _run_gh(["release", "download", manifest["tag"], "--repo", repository, "--dir", str(downloaded)], gh_executable)
        for asset in checked:
            path = downloaded / asset["name"]
            if not expected_draft:
                _download_public_asset(asset, path)
            info = _regular(path)
            if info.st_size != asset["sizeBytes"] or _sha256(path) != asset["sha256"]:
                raise AssetValidationError(f"Downloaded SHA-256/size mismatch: {asset['name']}")
            receipt["assets"].append(asset)
        if {path.name for path in downloaded.iterdir()} != {asset["name"] for asset in checked}:
            raise AssetValidationError("Downloaded files do not match the exact prepared assets")
        receipt["status"] = "verified"
        return receipt
    except Exception as error:
        receipt.update(status="failed", error=str(error))
        raise
    finally:
        receipt["completedAtUtc"] = datetime.now(timezone.utc).isoformat()
        _write_json(output / "receipt.json", receipt)


def _verify_local_to_directory(version, asset_directory, architectures, output_directory):
    output = _new_output_directory(output_directory)
    receipt = {"schema": SCHEMA, "mode": "local", "version": version, "status": "verifying"}
    try:
        manifest = validate_local_assets(version, asset_directory, architectures)
        _write_json(output / "local-manifest.json", manifest)
        receipt.update(status="verified", assets=manifest["files"])
        return receipt
    except Exception as error:
        receipt.update(status="failed", error=str(error))
        raise
    finally:
        _write_json(output / "receipt.json", receipt)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("local", "remote"))
    parser.add_argument("--version", required=True)
    parser.add_argument("--asset-directory", required=True)
    parser.add_argument("--output-directory", required=True)
    parser.add_argument("--architectures", nargs="+", choices=tuple(TARGETS), default=tuple(TARGETS))
    parser.add_argument("--repository", default=REPOSITORY)
    parser.add_argument("--draft", action="store_true", help="Expect a draft; use authenticated gh downloads instead of anonymous HTTPS")
    args = parser.parse_args(argv)
    try:
        if args.mode == "local":
            receipt = _verify_local_to_directory(args.version, args.asset_directory, args.architectures, args.output_directory)
        else:
            receipt = verify_remote_release(args.version, args.asset_directory, repository=args.repository,
                                            expected_draft=args.draft, architectures=args.architectures,
                                            output_directory=args.output_directory)
        print(json.dumps({"status": receipt["status"], "version": args.version, "files": len(receipt["assets"]),
                          "receipt": str(Path(args.output_directory).resolve() / "receipt.json")}, separators=(",", ":")))
        return 0
    except (AssetValidationError, OSError, UnicodeError, subprocess.SubprocessError) as error:
        print(json.dumps({"status": "failed", "version": args.version, "error": str(error),
                          "outputDirectory": str(Path(args.output_directory).resolve())}, separators=(",", ":")), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
