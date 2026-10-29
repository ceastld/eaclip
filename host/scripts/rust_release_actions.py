#!/usr/bin/env python3
"""Read-only verification of frozen native preview and owner share evidence.

For each registry action, evidence lives in ``<root>/<key's final segment>/``:
``preview.json`` is the exact native share-preview string encoded as UTF-8;
``owner.json`` is the unmodified ``quicker action get --id LOCAL_ID`` response.
``before-owner.json`` optionally proves that the public revision advanced.
The action directory's optional ``changelog.txt`` supplies the expected changelog,
with a compatible root-level file as fallback. Conflicting files are rejected.

This module performs no network access, subprocess execution, or account writes.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import stat
import sys
from typing import Any
from uuid import UUID


class ActionEvidenceError(ValueError):
    """Evidence is missing, ambiguous, malformed, or fails a release invariant."""


_MISSING = object()
_PLACEHOLDER_ATTRIBUTES = 0x1000 | 0x40000 | 0x400000
_REPARSE_ATTRIBUTE = 0x400
_BOOL_KEYS = {"handleActionParam", "connected_toggle_sent", "rust_bootstrap_complete"}


def _fail(message: str) -> None:
    raise ActionEvidenceError(message)


def _field(value: dict, key: str, default: Any = _MISSING) -> Any:
    """Accept native PascalCase and V2 camelCase, rejecting ambiguous aliases."""
    aliases = (key, key[0].upper() + key[1:])
    found = [alias for alias in aliases if alias in value]
    if len(found) > 1:
        _fail(f"Ambiguous field aliases: {key}")
    if found:
        return value[found[0]]
    if default is _MISSING:
        _fail(f"Missing field: {key}")
    return default


def _object(value: Any, label: str) -> dict:
    if not isinstance(value, dict):
        _fail(f"{label} must be an object")
    return value


def _array(value: Any, label: str) -> list:
    if not isinstance(value, list):
        _fail(f"{label} must be an array")
    return value


def _text(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        _fail(f"{label} must be a nonempty string")
    return value


def _uuid(value: Any, label: str) -> str:
    value = _text(value, label)
    try:
        return str(UUID(value))
    except ValueError:
        _fail(f"{label} must be a GUID")


def _version(value: Any, label: str, *, allow_three: bool = False) -> tuple[int, ...]:
    value = _text(value, label)
    pattern = r"[0-9]+\.[0-9]+\.[0-9]+(?:\.[0-9]+)?" if allow_three else r"[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+"
    if re.fullmatch(pattern, value) is None:
        _fail(f"{label} must be a {'three- or four' if allow_three else 'four'}-part version")
    parts = tuple(int(part) for part in value.split("."))
    if any(part > 2147483647 for part in parts):
        _fail(f"{label} contains an invalid version component")
    return parts if len(parts) == 4 else (*parts, 0)


def _unique_json_pairs(pairs: list[tuple[str, Any]]) -> dict:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            _fail(f"Duplicate JSON property: {key}")
        result[key] = value
    return result


def _parse_json(text: str, label: str) -> dict:
    try:
        value = json.loads(
            text,
            object_pairs_hook=_unique_json_pairs,
            parse_constant=lambda _: _fail(f"{label} contains a non-JSON number"),
        )
    except (json.JSONDecodeError, RecursionError) as error:
        raise ActionEvidenceError(f"{label} is not valid JSON") from error
    return _object(value, label)


def _is_network_path(path: Path) -> bool:
    # Reject UNC and device namespaces before issuing any filesystem query.
    if str(path).startswith(("\\\\", "//")):
        return True
    if sys.platform == "win32":
        import ctypes

        get_drive_type = ctypes.WinDLL("kernel32", use_last_error=True).GetDriveTypeW
        get_drive_type.argtypes = [ctypes.c_wchar_p]
        get_drive_type.restype = ctypes.c_uint
        drive_type = get_drive_type(path.anchor)
        if drive_type in (0, 1):
            _fail("Cannot establish a local evidence drive")
        return drive_type == 4  # DRIVE_REMOTE, including mapped network drives.
    return False


def _safe_metadata(path: Path, *, missing_ok: bool = False):
    """Check each ancestor with lstat before accessing any child or file bytes."""
    path = Path(path)
    if ".." in path.parts:
        _fail("Parent traversal is not accepted in evidence paths")
    absolute = path.absolute()
    if _is_network_path(absolute):
        _fail("Network evidence paths are not read")
    if any(part.casefold().startswith(("onedrive", "dropbox", "icloud")) for part in absolute.parent.parts):
        _fail("Cloud synchronization directories are not read")
    chain = [*reversed(absolute.parents), absolute]
    try:
        for component in chain:
            # Never resolve() or stat(): either could follow an unchecked link.
            metadata = component.lstat()
            attributes = getattr(metadata, "st_file_attributes", 0)
            if stat.S_ISLNK(metadata.st_mode):
                _fail("Symbolic links in evidence paths are not read")
            if attributes & _REPARSE_ATTRIBUTE:
                _fail("Reparse points in evidence paths are not read")
            if attributes & _PLACEHOLDER_ATTRIBUTES:
                _fail("Offline or Recall evidence paths are not read")
            if component != absolute and not stat.S_ISDIR(metadata.st_mode):
                _fail("Evidence ancestor is not a directory")
        if not stat.S_ISREG(metadata.st_mode):
            _fail(f"Evidence is not a regular file: {path.name}")
        return metadata
    except FileNotFoundError as error:
        if missing_ok:
            return None
        raise ActionEvidenceError(f"Cannot read evidence file: {path.name}") from error
    except OSError as error:
        raise ActionEvidenceError(f"Cannot inspect evidence path: {path.name}") from error


def _exists_safely(path: Path) -> bool:
    return _safe_metadata(path, missing_ok=True) is not None


def _read_bytes(path: Path) -> bytes:
    _safe_metadata(path)
    try:
        data = path.read_bytes()
    except OSError as error:
        raise ActionEvidenceError(f"Cannot read evidence file: {path.name}") from error
    if not data:
        _fail(f"Evidence file is empty: {path.name}")
    return data


def _decode(data: bytes, label: str) -> str:
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ActionEvidenceError(f"{label} is not UTF-8") from error


def _read_json(path: Path) -> dict:
    return _parse_json(_decode(_read_bytes(path), path.name), path.name)


def _normalize_changelog(value: str) -> str:
    # Only line endings and terminal line breaks are normalized. In particular,
    # leading/trailing spaces on content lines remain part of the expectation.
    return value.replace("\r\n", "\n").rstrip("\n")


def _expected_changelog(action: dict, directory: Path, evidence_root: Path) -> str | None:
    per_action_path = directory / "changelog.txt"
    root_path = evidence_root / "changelog.txt"
    per_action = _decode(_read_bytes(per_action_path), "action changelog.txt") if _exists_safely(per_action_path) else None
    root = _decode(_read_bytes(root_path), "root changelog.txt") if _exists_safely(root_path) else None
    if per_action is not None and root is not None:
        if _normalize_changelog(per_action) != _normalize_changelog(root):
            _fail("Action and root changelog.txt files conflict")
    selected = per_action if per_action is not None else root
    explicit = action.get("expectedChangelog")
    if explicit is not None:
        if not isinstance(explicit, str):
            _fail("expectedChangelog must be a string")
        if selected is not None and _normalize_changelog(explicit) != _normalize_changelog(selected):
            _fail("expectedChangelog conflicts with the frozen changelog.txt")
        if selected is None:
            selected = explicit
    return selected


def _owner(response: dict, action: dict, label: str) -> tuple[dict, int, str]:
    if _field(response, "isSuccess") is not True:
        _fail(f"{label} reports an unsuccessful API response")
    error_code = _field(response, "errorCode", 0)
    if type(error_code) is not int or error_code != 0:
        _fail(f"{label} contains an API error")
    payload = _object(_field(response, "data"), f"{label}.data")
    if _uuid(_field(payload, "id"), f"{label}.id") != _uuid(action["sharedActionId"], "sharedActionId"):
        _fail(f"{label} shared identity does not match registry")
    if _uuid(_field(payload, "internalId"), f"{label}.internalId") != _uuid(action["actionId"], "actionId"):
        _fail(f"{label} local identity does not match registry")
    if _field(payload, "title") != action["title"]:
        _fail(f"{label} title does not match registry")
    if _field(payload, "isPublic") is not True:
        _fail(f"{label} is not public")
    if _field(payload, "isDeleted", False) is not False:
        _fail(f"{label} is deleted")
    revision = _field(payload, "revision")
    if type(revision) is not int or revision < 0:
        _fail(f"{label} revision must be a nonnegative integer")
    body = _text(_field(payload, "data"), f"{label}.data.data")
    return payload, revision, body


def _programs(body: dict) -> list[dict]:
    """Collect definitions from the raw arrays BEFORE constructing any maps."""
    definitions: list[dict] = []
    ids: set[str] = set()
    names: set[str] = set()

    def collect(program: dict) -> None:
        _array(_field(program, "steps"), "program.steps")
        _array(_field(program, "variables"), "program.variables")
        for child in _array(_field(program, "subPrograms"), "program.subPrograms"):
            child = _object(child, "subprogram")
            child_id = _uuid(_field(child, "id"), "subprogram.id")
            name = _text(_field(child, "name"), "subprogram.name")
            if child_id in ids:
                _fail("Duplicate raw subprogram ID")
            if name.casefold() in names:
                _fail("Duplicate raw subprogram name")
            ids.add(child_id)
            names.add(name.casefold())
            definitions.append(child)
            collect(child)

    collect(body)
    return definitions


def _steps(program: dict):
    """Yield steps and conditional branches, without crossing program scopes."""
    def visit(steps: list):
        for step in steps:
            step = _object(step, "step")
            yield step
            for branch in ("ifSteps", "elseSteps"):
                children = _field(step, branch, None)
                if children is not None:
                    yield from visit(_array(children, branch))

    yield from visit(_array(_field(program, "steps"), "program.steps"))


def _literal(inputs: dict, key: str) -> Any:
    cell = _object(inputs.get(key), f"input {key}")
    if _field(cell, "varKey", None) not in (None, ""):
        _fail(f"input {key} must be a static literal")
    return _field(cell, "value")


def _variables(program: dict) -> dict[str, dict]:
    variables: dict[str, dict] = {}
    for variable in _array(_field(program, "variables"), "program.variables"):
        variable = _object(variable, "variable")
        key = _text(_field(variable, "key"), "variable.key")
        if key in variables:
            _fail("Duplicate variable key")
        variables[key] = variable
        native_type = _field(variable, "type")
        if type(native_type) is not int:
            _fail(f"Variable {key} lacks a native integer type")
        default = _field(variable, "defaultValue", None)
        looks_boolean = type(default) is bool or (isinstance(default, str) and default.lower() in ("true", "false"))
        if (key in _BOOL_KEYS or looks_boolean) and native_type != 2:
            _fail(f"Boolean variable {key} must retain native type 2")
    return variables


def _version_variable(variables: dict[str, dict], key: str, label: str) -> tuple[int, ...]:
    if key not in variables:
        _fail(f"{label} is missing {key}")
    variable = variables[key]
    if _field(variable, "type") != 0:
        _fail(f"{label}.{key} must retain native text type 0")
    return _version(_field(variable, "defaultValue"), f"{label}.{key}")


def verify_action_evidence(action: dict, version: str, evidence_directory: Path) -> dict:
    """Verify one action's immutable evidence and return compact aggregate data."""
    action = _object(action, "action")
    for key in ("key", "title", "actionId", "sharedActionId", "command"):
        _text(action.get(key), f"action.{key}")
    name = action["key"].rsplit("/", 1)[-1]
    if re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name) is None:
        _fail("Invalid evidence directory name in action key")
    target = _version(version, "target version", allow_three=True)
    directory = Path(evidence_directory) / name
    preview_bytes = _read_bytes(directory / "preview.json")
    preview = _parse_json(_decode(preview_bytes, "preview.json"), "preview.json")
    owner, revision, public_body = _owner(_read_json(directory / "owner.json"), action, "owner")
    try:
        public_bytes = public_body.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ActionEvidenceError("Public body is not valid UTF-8") from error
    if preview_bytes != public_bytes:
        _fail("Public body UTF-8 bytes do not exactly match the native preview")
    # Parse independently to reject invalid or ambiguous public JSON as well.
    _parse_json(public_body, "public body")

    before_revision = None
    before_path = directory / "before-owner.json"
    if _exists_safely(before_path):
        _, before_revision, _ = _owner(_read_json(before_path), action, "before-owner")
        if revision <= before_revision:
            _fail("Public revision did not advance from before-owner")

    definitions = _programs(preview)
    # These maps are constructed only after the raw recursive uniqueness check.
    by_id = {_uuid(_field(item, "id"), "subprogram.id"): item for item in definitions}
    by_name = {_field(item, "name").casefold(): item for item in definitions}
    for required in ("CeaCore_Run", "ClipHost"):
        if required.casefold() not in by_name:
            _fail(f"Missing embedded runtime subprogram: {required}")

    def resolve(reference: str) -> dict:
        reference = _text(reference, "subprogram reference")
        if reference.startswith("%%"):
            return by_id.get(_uuid(reference[2:], "global subprogram reference")) or _undefined()
        candidates = []
        if reference.casefold() in by_name:
            candidates.append(by_name[reference.casefold()])
        try:
            identifier = str(UUID(reference))
        except ValueError:
            identifier = ""
        if identifier in by_id and all(item is not by_id[identifier] for item in candidates):
            candidates.append(by_id[identifier])
        if len(candidates) != 1:
            _fail("Undefined or ambiguous embedded subprogram reference")
        return candidates[0]

    def calls(program: dict):
        for step in _steps(program):
            if _field(step, "stepRunnerKey") == "sys:subprogram":
                inputs = _object(_field(step, "inputParams"), "step.inputParams")
                yield resolve(_literal(inputs, "subProgram")), inputs

    reachable: set[str] = set()
    pending = [preview]
    root_calls = list(calls(preview))
    core = by_name["ceacore_run"]
    root_core_calls = [inputs for item, inputs in root_calls if item is core]
    if len(root_core_calls) != 1:
        _fail("Root program must call CeaCore_Run exactly once")
    root_inputs = root_core_calls[0]
    if _literal(root_inputs, "var:cmd") != action["command"]:
        _fail("Root command does not match the registered command")
    if _literal(root_inputs, "var:host_backend") != "rust":
        _fail("Root host_backend must be rust")
    handle_action_param = _literal(root_inputs, "var:handleActionParam")
    if handle_action_param is not True and handle_action_param != "true":
        _fail("Root handleActionParam must be true")
    # Validate every definition, including ones not reached, before checking closure.
    adjacency = {id(program): list(calls(program)) for program in [preview, *definitions]}
    while pending:
        for child, _ in adjacency[id(pending.pop())]:
            identifier = _uuid(_field(child, "id"), "subprogram.id")
            if identifier not in reachable:
                reachable.add(identifier)
                pending.append(child)
    if len(reachable) != len(definitions):
        _fail("Embedded subprogram closure contains unreachable definitions")

    variables = {id(program): _variables(program) for program in [preview, *definitions]}
    runtime_versions = {}
    for required in ("CeaCore_Run", "ClipHost"):
        program = by_name[required.casefold()]
        actual = _version_variable(variables[id(program)], "rust_version", required)
        if actual != target:
            _fail(f"{required}.rust_version does not match the target host version")
        runtime_versions[required] = ".".join(map(str, actual))
    core_variables = variables[id(core)]
    if "handleActionParam" not in core_variables:
        _fail("CeaCore_Run is missing its native boolean handleActionParam variable")
    launcher = _version_variable(core_variables, "rust_launcher_version", "CeaCore_Run")
    expected_launcher = action.get("expectedLauncherVersion")
    if expected_launcher is not None and launcher != _version(expected_launcher, "expectedLauncherVersion"):
        _fail("Launcher version does not match expectedLauncherVersion")
    minimum_launcher = action.get("minimumLauncherVersion")
    if minimum_launcher is not None and launcher < _version(minimum_launcher, "minimumLauncherVersion"):
        _fail("Launcher version is below minimumLauncherVersion")
    # Launcher and host can legitimately share a version; they are independently
    # checked against their explicit expectations, never inferred from each other.
    host_variables = variables[id(by_name["cliphost"])]
    if "rust_launcher_version" in host_variables:
        if _version_variable(host_variables, "rust_launcher_version", "ClipHost") != launcher:
            _fail("Compatibility Host launcher version differs from Core")

    changelog = _expected_changelog(action, directory, Path(evidence_directory))
    if changelog is not None:
        public_changelog = _field(owner, "changeLog")
        if not isinstance(public_changelog, str) or _normalize_changelog(public_changelog) != _normalize_changelog(changelog):
            _fail("Public changelog does not match the frozen expected changelog")

    return {
        "status": "verified",
        "key": action["key"],
        "title": action["title"],
        "actionId": action["actionId"],
        "sharedActionId": action["sharedActionId"],
        "revision": revision,
        "beforeRevision": before_revision,
        "isPublic": True,
        "command": action["command"],
        "hostVersion": runtime_versions["CeaCore_Run"],
        "launcherVersion": ".".join(map(str, launcher)),
        "bodySha256": hashlib.sha256(public_bytes).hexdigest(),
        "bodyUtf8Bytes": len(public_bytes),
        "rawSubProgramCount": len(definitions),
        "uniqueIds": len(by_id),
        "uniqueNames": len(by_name),
        "changelogVerified": changelog is not None,
    }


def _undefined():
    _fail("Undefined embedded global subprogram reference")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--action-file", type=Path, required=True, help="One registry action as JSON")
    parser.add_argument("--version", required=True, help="Target Rust host version")
    parser.add_argument("--evidence-directory", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        result = verify_action_evidence(_read_json(args.action_file), args.version, args.evidence_directory)
    except ActionEvidenceError as error:
        print(json.dumps({"status": "failed", "error": str(error)}, ensure_ascii=True))
        return 1
    print(json.dumps(result, ensure_ascii=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
