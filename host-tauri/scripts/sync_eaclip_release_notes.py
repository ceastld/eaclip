"""Mirror public eaclip Release bodies and user docs into eaclip; dry-run by default.

Only README.md, CHANGELOG.md, release-notes/rust-v<version>.md and docs/** can
change. docs/ mirrors this repository's docs/user/ exactly (files no longer in
the source are deleted, only inside docs/). --source-in-destination protects
docs/user/ when the destination repository also contains these source files.
Every mirrored document must pass
the public-content check (no private repository, source paths, local paths or
credential-like strings; relative links stay inside docs/). README gains one
idempotent "docs/README.md" link outside the release markers.
No Release, asset or tag is created or modified. All gh calls use argument lists.
--local-checkout applies to a local eaclip clone's working tree for trial runs
(no commit, no push); --check-docs validates docs/user/ without network access.
"""

import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path, PurePosixPath
import posixpath
import re
import subprocess
import sys
from urllib.parse import quote
import uuid


REPOSITORY = "ceastld/eaclip"
TAG = re.compile(r"rust-v(\d+)\.(\d+)\.(\d+)")
SHA = re.compile(r"[0-9a-f]{40}")
START = "<!-- eaclip:releases:start -->"
END = "<!-- eaclip:releases:end -->"
WORKSPACE = Path(__file__).resolve().parents[2]
DOCS_SOURCE = WORKSPACE / "docs" / "user"
DOCS_PREFIX = "docs/"
DOCS_LINK_TARGET = "](docs/README.md)"
DOCS_LINK_LINE = "- [使用说明与常见问题](docs/README.md)"
LINK = re.compile(r"!?\[[^\]]*\]\(<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\)")
FORBIDDEN = (
    (re.compile(r"CeaQuickerTools", re.IGNORECASE), "private source repository"),
    (re.compile(r"quicker-workspace", re.IGNORECASE), "private workspace"),
    (re.compile(r"(?<![\w.-])(?:host-tauri|frontend|crates|legacy|src|\.agents|\.github|\.tmp)/"), "source tree path"),
    (re.compile(r"\b[\w-]+\.(?:rs|py|ps1|psm1|csproj|tsx?|mdx)\b"), "source file name"),
    (re.compile(r"\b[A-Za-z]:\\"), "absolute Windows path"),
    (re.compile(r"\\Users\\|/home/|/Users/"), "user profile path"),
    (re.compile(r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16})"), "credential-like token"),
    (re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"), "private key"),
    (re.compile(r"release-notes/public|actions/runs/"), "internal release record"),
)


class SyncError(RuntimeError):
    pass


def digest(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def git_blob_sha(text):
    data = text.encode("utf-8")
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def check_public_document(path, text, available):
    for pattern, reason in FORBIDDEN:
        match = pattern.search(text)
        if match:
            raise SyncError(f"{path} contains a {reason} ({match.group(0)!r}); fix docs/user before publishing")
    base = str(PurePosixPath(path).parent)
    for match in LINK.finditer(text):
        target = match.group(1)
        if target.startswith("#") or re.match(r"(?i)(?:https?|mailto):", target):
            continue
        if re.match(r"[A-Za-z][A-Za-z0-9+.-]*:", target):
            raise SyncError(f"{path} links to an unsupported scheme: {target}")
        resolved = posixpath.normpath(posixpath.join(base, target.split("#", 1)[0]))
        if resolved not in available:
            raise SyncError(f"{path} links outside the mirrored docs or to a missing file: {target}")


def public_docs(source=None):
    """Read docs/user/ as eaclip docs/ paths; reject anything unfit for the public repository."""
    source = Path(DOCS_SOURCE if source is None else source)
    if source.is_symlink() or not source.is_dir():
        raise SyncError("docs/user must be a regular directory")
    files = {}
    for item in sorted(source.rglob("*")):
        relative = item.relative_to(source).as_posix()
        if item.is_symlink():
            raise SyncError(f"Symlinks are not mirrored: {relative}")
        if item.is_dir():
            continue
        if item.suffix.lower() != ".md":
            raise SyncError(f"Only Markdown is mirrored to eaclip docs/: {relative}")
        try:
            text = item.read_bytes().decode("utf-8")
        except UnicodeError as error:
            raise SyncError(f"Expected UTF-8 Markdown: {relative}") from error
        files[DOCS_PREFIX + relative] = text.replace("\r\n", "\n")
    if DOCS_PREFIX + "README.md" not in files:
        raise SyncError("docs/user/README.md is required as the eaclip docs entry")
    for path, text in files.items():
        check_public_document(path, text, files)
    return files


def same_document(entry, content):
    if entry.get("local"):
        # Local clones may hold CRLF working copies (core.autocrlf); compare normalized text.
        return entry.get("content") == content.replace("\r\n", "\n")
    if "content" in entry:
        return entry["content"] == content
    return entry.get("sha") == git_blob_sha(content)


def is_docs_source(path):
    """Reserve the source subtree, including case aliases in Windows clones."""
    path = path.casefold()
    return path == "docs/user" or path.startswith("docs/user/")


def checked_sha(value):
    if not isinstance(value, str) or not SHA.fullmatch(value):
        raise SyncError("GitHub returned an invalid Git object SHA")
    return value


def api(endpoint, *, method="GET", data=None, paginate=False):
    args = ["gh", "api", "--hostname", "github.com", "--method", method,
            f"repos/{REPOSITORY}/{endpoint}".rstrip("/")]
    if paginate:
        args.extend(["--paginate", "--slurp"])
    if data is not None:
        args.extend(["--input", "-"])
    try:
        result = subprocess.run(
            args, input=None if data is None else json.dumps(data, ensure_ascii=False),
            encoding="utf-8", capture_output=True, check=False, timeout=90,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise SyncError(f"{method} {endpoint} did not return a known result; do not retry writes blindly") from error
    if result.returncode:
        # Do not print command output, account details or environment variables.
        raise SyncError(f"gh API {method} {endpoint} failed (exit {result.returncode}); inspect remote state before retrying")
    try:
        return json.loads(result.stdout)
    except (ValueError, TypeError) as error:
        raise SyncError(f"Invalid JSON response to {method} {endpoint}") from error


def repository_branch():
    repository = api("")
    branch = repository.get("default_branch")
    if repository.get("private") is not False or not isinstance(branch, str) or not branch:
        raise SyncError("Expected the public eaclip repository with an existing default branch")
    return branch


def branch_head(branch):
    ref = api("git/ref/heads/" + quote(branch, safe=""))
    if ref.get("ref") != f"refs/heads/{branch}" or ref.get("object", {}).get("type") != "commit":
        raise SyncError("Unexpected default branch ref")
    return checked_sha(ref["object"].get("sha"))


def public_releases(selected_tag):
    pages = api("releases?per_page=100", paginate=True)
    if not isinstance(pages, list) or any(not isinstance(page, list) for page in pages):
        raise SyncError("Invalid paginated Release listing")
    releases = []
    seen = set()
    for release in (release for page in pages for release in page):
        tag = release.get("tag_name", "")
        match = TAG.fullmatch(tag)
        if not match or release.get("draft") is not False or not release.get("published_at"):
            continue
        body = release.get("body")
        if not isinstance(body, str) or not body.strip():
            if tag == selected_tag:
                raise SyncError(f"Selected public Release {tag} has an empty body")
            continue
        if tag in seen:
            raise SyncError(f"Duplicate published Release: {tag}")
        seen.add(tag)
        url = f"https://github.com/{REPOSITORY}/releases/tag/{tag}"
        if release.get("html_url") != url:
            raise SyncError(f"Unexpected public Release URL for {tag}")
        version = ".".join(match.groups())
        downloads = []
        for arch in ("x64", "arm64"):
            name = f"eaclip-{version}-windows-{arch}.zip"
            assets = [asset for asset in release.get("assets", []) if asset.get("name") == name]
            if len(assets) > 1:
                raise SyncError(f"Duplicate portable asset in {tag}: {name}")
            if assets:
                asset = assets[0]
                expected_url = f"https://github.com/{REPOSITORY}/releases/download/{tag}/{name}"
                if (asset.get("state") != "uploaded" or asset.get("browser_download_url") != expected_url
                        or not isinstance(asset.get("size"), int) or asset["size"] <= 0):
                    raise SyncError(f"Invalid portable asset in {tag}: {name}")
                downloads.append({"architecture": arch, "name": name, "url": expected_url,
                                  "id": asset.get("id"), "sizeBytes": asset["size"],
                                  "digest": asset.get("digest")})
        if not downloads:
            raise SyncError(f"No uploaded Windows portable asset in public Release {tag}")
        releases.append({"tag": tag, "version": version, "id": release.get("id"),
                         "publishedAt": release["published_at"], "prerelease": release.get("prerelease", False),
                         "url": url, "body": body, "downloads": downloads})
    if not any(release["tag"] == selected_tag for release in releases):
        raise SyncError(f"Selected Release {selected_tag} is missing, unpublished or a draft")
    return sorted(releases, key=lambda release: tuple(map(int, TAG.fullmatch(release["tag"]).groups())), reverse=True)


def read_documents(head):
    commit = api(f"git/commits/{head}")
    tree_sha = checked_sha(commit.get("tree", {}).get("sha"))
    tree = api(f"git/trees/{tree_sha}?recursive=1")
    if tree.get("truncated") is not False:
        raise SyncError("Cannot safely inspect a truncated repository tree")
    entries = {}
    for entry in tree.get("tree", []):
        path = entry.get("path", "")
        if path == "README.md" or path == "CHANGELOG.md" or re.fullmatch(r"release-notes/rust-v\d+\.\d+\.\d+\.md", path):
            if path in entries or entry.get("type") != "blob" or entry.get("mode") not in ("100644", "100755"):
                raise SyncError(f"Expected a regular, unique documentation file: {path}")
            blob = api(f"git/blobs/{checked_sha(entry.get('sha'))}")
            if blob.get("encoding") != "base64":
                raise SyncError(f"Unexpected blob encoding: {path}")
            try:
                content = base64.b64decode(blob["content"]).decode("utf-8")
            except (KeyError, ValueError, UnicodeError) as error:
                raise SyncError(f"Expected UTF-8 documentation: {path}") from error
            entries[path] = {"sha": entry["sha"], "mode": entry["mode"], "content": content}
        elif path.startswith(DOCS_PREFIX):
            if entry.get("type") == "tree":
                continue
            if path in entries or entry.get("type") != "blob":
                raise SyncError(f"Unexpected entry under docs/: {path}")
            # Compared by Git blob id; content is only needed when it changes.
            entries[path] = {"sha": checked_sha(entry.get("sha")), "mode": entry.get("mode")}
    if "README.md" not in entries:
        raise SyncError("The existing README.md is required; it will not be replaced with a new one")
    return tree_sha, entries


def read_local_documents(root):
    """Local-clone equivalent of read_documents for --local-checkout trial runs."""
    candidates = [root / "README.md", root / "CHANGELOG.md"]
    if (root / "release-notes").is_dir():
        candidates += sorted(item for item in (root / "release-notes").iterdir()
                             if re.fullmatch(r"rust-v\d+\.\d+\.\d+\.md", item.name))
    if (root / "docs").is_dir():
        candidates += sorted(item for item in (root / "docs").rglob("*") if not item.is_dir())
    entries = {}
    for item in candidates:
        if not item.exists():
            continue
        path = item.relative_to(root).as_posix()
        if item.is_symlink() or not item.is_file():
            raise SyncError(f"Expected a regular documentation file: {path}")
        try:
            content = item.read_bytes().decode("utf-8").replace("\r\n", "\n")
        except UnicodeError:
            entries[path] = {"sha": "binary", "mode": "100644"}
            continue
        entries[path] = {"sha": git_blob_sha(content), "mode": "100644", "content": content, "local": True}
    if "README.md" not in entries:
        raise SyncError("The existing README.md is required; it will not be replaced with a new one")
    return entries


def write_local(root, changes, deletions):
    for path, content in changes.items():
        target = (root / path).resolve()
        if not target.is_relative_to(root):
            raise SyncError(f"Refusing to write outside the local checkout: {path}")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content.encode("utf-8"))
    for path in deletions:
        target = (root / path).resolve()
        if not path.startswith(DOCS_PREFIX) or not target.is_relative_to(root / "docs"):
            raise SyncError(f"Refusing to delete outside docs/: {path}")
        target.unlink()


def ensure_docs_link(original):
    """Keep exactly one docs/README.md link outside the release block (idempotent)."""
    start = original.find(START)
    head, tail = (original, "") if start < 0 else (original[:start], original[start:])
    finish = original.find(END)
    outside = head + (original[finish + len(END):] if finish >= 0 else "")
    if DOCS_LINK_TARGET in outside:
        return original
    newline = "\r\n" if "\r\n" in original else "\n"
    lines = head.split(newline)
    bullets = [index for index, line in enumerate(lines) if line.startswith("- [")]
    if bullets:
        lines.insert(bullets[-1] + 1, DOCS_LINK_LINE)
        head = newline.join(lines)
    else:
        body = head.rstrip("\r\n")
        head = (body + newline * 2 if body else "") + DOCS_LINK_LINE + newline + (newline if tail else "")
    return head + tail


def update_readme(original, block):
    starts, ends = original.count(START), original.count(END)
    if starts == ends == 0:
        separator = "" if original.endswith("\n\n") else "\n" if original.endswith("\n") else "\n\n"
        return original + separator + block + "\n"
    if starts != 1 or ends != 1:
        raise SyncError("README has incomplete or duplicate eaclip Release markers")
    start, end = original.index(START), original.index(END)
    finish = end + len(END)
    if (start >= end or (start and original[start - 1] != "\n")
            or (finish < len(original) and original[finish] not in "\r\n")):
        raise SyncError("README Release markers must delimit a standalone block")
    return original[:start] + block + original[finish:]


def candidate_documents(releases, readme, docs=None):
    latest = releases[0]
    label = {"x64": "x64（Intel / AMD 64 位）", "arm64": "ARM64"}
    block = [START, "## 下载与更新", "",
             f"当前版本：[{latest['version']}]({latest['url']})", "",
             "| Windows 架构 | 便携包 |", "| --- | --- |"]
    for asset in latest["downloads"]:
        block.append(f"| {label[asset['architecture']]} | [下载 ZIP]({asset['url']}) |")
    block.extend(["", "按 Windows 系统架构选择便携包，完整解压后运行 `cliphost-app.exe`，保留同目录 DLL。", "",
                  f"[最新更新说明](release-notes/{latest['tag']}.md) · [全部版本](CHANGELOG.md) · "
                  f"[GitHub Releases](https://github.com/{REPOSITORY}/releases)", END])
    files = {"README.md": update_readme(ensure_docs_link(readme), "\n".join(block))}
    files.update(docs or {})
    changelog = ["# 更新记录", "", "各版本更新说明与公开下载：", ""]
    for release in releases:
        tag = release["tag"]
        body = release["body"]
        files[f"release-notes/{tag}.md"] = body if body.endswith("\n") else body + "\n"
        architectures = " / ".join(asset["architecture"] for asset in release["downloads"])
        preview = "，预览版" if release["prerelease"] else ""
        changelog.append(f"- [{release['version']}](release-notes/{tag}.md) · [下载]({release['url']})"
                         f"（Windows {architectures}{preview}）")
    files["CHANGELOG.md"] = "\n".join(changelog) + "\n"
    return files


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def sync(selected_tag, output_directory, *, apply=False, local_checkout=None, source_in_destination=False):
    if not TAG.fullmatch(selected_tag):
        raise SyncError("Expected a rust-v<major>.<minor>.<patch> tag")
    output = Path(output_directory).resolve()
    temporary_root = (WORKSPACE / ".tmp").resolve()
    if output == temporary_root or not output.is_relative_to(temporary_root):
        raise SyncError("Output directory must be a dedicated directory inside this repository's .tmp")
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        raise SyncError("Output directory must be empty; previous receipts will not be overwritten")
    receipt_path = output / "receipt.json"
    receipt = {"repository": REPOSITORY, "selectedTag": selected_tag, "apply": apply,
               "sourceInDestination": source_in_destination,
               "startedAtUtc": datetime.now(timezone.utc).isoformat(), "status": "preparing",
               "releaseAndAssetsModified": False, "refUpdateAttempted": False}
    write_json(receipt_path, receipt)
    try:
        docs = public_docs()
        if source_in_destination and any(is_docs_source(path) for path in docs):
            raise SyncError("Mirrored documentation overlaps the protected docs/user source directory")
        if local_checkout is None:
            branch = repository_branch()
            head = branch_head(branch)
            releases = public_releases(selected_tag)
            tree, existing = read_documents(head)
        else:
            local = Path(local_checkout).resolve()
            if not (local / ".git").exists() or local == WORKSPACE or local.is_relative_to(WORKSPACE):
                raise SyncError("--local-checkout must be a separate eaclip clone outside this repository")
            branch, head, tree = "local-checkout", None, None
            releases = public_releases(selected_tag)
            existing = read_local_documents(local)
            receipt["localCheckout"] = str(local)
        files = candidate_documents(releases, existing["README.md"]["content"], docs)
        changes = {path: content for path, content in sorted(files.items())
                   if path not in existing or not same_document(existing[path], content)}
        deletions = sorted(path for path in existing if path.startswith(DOCS_PREFIX) and path not in files
                           and not (source_in_destination and is_docs_source(path)))
        receipt.update(defaultBranch=branch, baseCommit=head, baseTree=tree,
                       changedPaths=list(changes), deletedPaths=deletions,
                       publicTags=[release["tag"] for release in releases])
        for path, content in files.items():
            destination = output / "candidate" / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(content.encode("utf-8"))
        plan = {"repository": REPOSITORY, "branch": branch, "baseCommit": head,
                "sourceInDestination": source_in_destination,
                "selectedTag": selected_tag, "changes": [
                    {"path": path, "sha256": digest(content),
                     "previousBlob": existing.get(path, {}).get("sha")}
                    for path, content in changes.items()],
                "deletions": [{"path": path, "previousBlob": existing[path]["sha"]} for path in deletions],
                "releases": [{key: value for key, value in release.items() if key != "body"}
                             | {"bodySha256": digest(release["body"])} for release in releases]}
        write_json(output / "plan.json", plan)
        if not apply or not (changes or deletions):
            receipt["status"] = "dry-run" if not apply else "unchanged"
            return receipt
        if local_checkout is not None:
            write_local(local, changes, deletions)
            receipt["status"] = "applied-local"
            return receipt

        def assert_inputs_current():
            if repository_branch() != branch:
                raise SyncError("Default branch changed; inspect and prepare a new candidate")
            if public_releases(selected_tag) != releases:
                raise SyncError("Public Release inputs changed; inspect and prepare a new candidate")
            if branch_head(branch) != head:
                raise SyncError("Default branch advanced; refusing a stale documentation commit")

        assert_inputs_current()
        receipt["status"] = "creating-tree"
        write_json(receipt_path, receipt)
        new_tree = api("git/trees", method="POST", data={"base_tree": tree, "tree": [
            {"path": path, "mode": existing.get(path, {}).get("mode", "100644"),
             "type": "blob", "content": content} for path, content in changes.items()]
            + [{"path": path, "mode": existing[path].get("mode") or "100644", "type": "blob", "sha": None}
               for path in deletions]})
        new_tree_sha = checked_sha(new_tree.get("sha"))
        receipt.update(status="creating-commit", newTree=new_tree_sha)
        write_json(receipt_path, receipt)
        commit = api("git/commits", method="POST", data={
            "message": f"docs: sync public release notes for {selected_tag}",
            "tree": new_tree_sha, "parents": [head]})
        commit_sha = checked_sha(commit.get("sha"))
        receipt.update(status="checking-ref", commit=commit_sha)
        write_json(receipt_path, receipt)
        # The commit has one parent: the observed base. A concurrent forward update
        # after this check cannot fast-forward to this sibling commit (force=false).
        assert_inputs_current()
        receipt.update(status="updating-ref", refUpdateAttempted=True)
        write_json(receipt_path, receipt)
        try:
            updated = api("git/refs/heads/" + quote(branch, safe=""), method="PATCH",
                          data={"sha": commit_sha, "force": False})
            if updated.get("object", {}).get("sha") != commit_sha:
                raise SyncError("Unexpected ref-update response; inspect remote state")
        except SyncError:
            # One read can establish that a timed-out write succeeded. Never resend
            # a mutation, create another commit or automatically rebase on conflicts.
            receipt["status"] = "ref-result-unknown"
            try:
                receipt["observedHead"] = branch_head(branch)
            except SyncError:
                pass
            if receipt.get("observedHead") != commit_sha:
                raise
            receipt["status"] = "applied-confirmed-after-error"
        else:
            receipt["observedHead"] = branch_head(branch)
            if receipt["observedHead"] != commit_sha:
                receipt["status"] = "ref-result-unknown"
                raise SyncError("Default branch no longer matches the written commit; inspect remote state")
            receipt["status"] = "applied"
        receipt["commitUrl"] = f"https://github.com/{REPOSITORY}/commit/{commit_sha}"
        return receipt
    except Exception as error:
        receipt["failedPhase"] = receipt["status"]
        receipt["status"] = "failed"
        receipt["error"] = str(error)
        raise
    finally:
        receipt["completedAtUtc"] = datetime.now(timezone.utc).isoformat()
        write_json(receipt_path, receipt)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tag", help="Selected, already public rust-v<version> Release")
    parser.add_argument("--output-directory", help="New/empty directory under this repository's .tmp")
    parser.add_argument("--apply", action="store_true", help="Commit the documentation to eaclip; otherwise only prepare candidates")
    parser.add_argument("--local-checkout", help="Trial run: apply to this local eaclip clone's working tree (no commit, no push)")
    parser.add_argument("--source-in-destination", action="store_true",
                        help="Preserve docs/user/ source files in a destination that also contains the product source")
    parser.add_argument("--check-docs", action="store_true", help="Only validate docs/user/ for publication (no network)")
    arguments = parser.parse_args()
    if arguments.check_docs:
        try:
            docs = public_docs()
        except (SyncError, OSError) as error:
            print(f"Public docs check failed: {error}", file=sys.stderr)
            return 1
        print(json.dumps({"status": "docs-ok", "files": sorted(docs)}, ensure_ascii=False))
        return 0
    if not arguments.tag:
        parser.error("--tag is required unless --check-docs is used")
    output = arguments.output_directory or WORKSPACE / ".tmp" / f"eaclip-release-notes-{uuid.uuid4().hex[:12]}"
    try:
        receipt = sync(arguments.tag, output, apply=arguments.apply, local_checkout=arguments.local_checkout,
                       source_in_destination=arguments.source_in_destination)
    except (SyncError, OSError, ValueError) as error:
        print(f"Release documentation sync stopped: {error}", file=sys.stderr)
        return 1
    print(json.dumps({"status": receipt["status"], "receipt": str(Path(output).resolve() / "receipt.json"),
                      "changedPaths": receipt["changedPaths"], "deletedPaths": receipt["deletedPaths"],
                      "commitUrl": receipt.get("commitUrl")}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
