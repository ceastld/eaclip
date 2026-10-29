"""Isolated gh API contract tests. No network or real repository writes."""

import base64
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("release_notes_sync", Path(__file__).with_name("sync_eaclip_release_notes.py"))
SYNC = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SYNC)


def release(version, architectures=("x64",), *, draft=False, body=None):
    tag = "rust-v" + version
    return {"id": int(version.split(".")[-1]), "tag_name": tag, "draft": draft,
            "published_at": None if draft else "2026-10-15T12:00:00Z", "prerelease": False,
            "body": f"# ClipHost {version}\n\n- Published feature.\n" if body is None else body,
            "html_url": f"https://github.com/{SYNC.REPOSITORY}/releases/tag/{tag}",
            "assets": [{"id": index + 100, "name": f"eaclip-{version}-windows-{arch}.zip",
                        "state": "uploaded", "size": 1234, "digest": "sha256:" + "f" * 64,
                        "browser_download_url": f"https://github.com/{SYNC.REPOSITORY}/releases/download/{tag}/"
                                                f"eaclip-{version}-windows-{arch}.zip"}
                       for index, arch in enumerate(architectures)]}


class FakeGh:
    base = "a" * 40
    base_tree = "b" * 40
    new_tree = "c" * 40
    commit = "d" * 40
    competing = "e" * 40

    def __init__(self):
        self.head = self.base
        self.branch = "main"
        self.releases = [release("0.31.12"), release("0.31.13", ("x64", "arm64")),
                         release("0.31.14", draft=True), {"tag_name": "unrelated-v1", "draft": False}]
        self.documents = {"README.md": "# eaclip\n\nOriginal description.\n"}
        self.calls = []
        self.head_reads = 0
        self.release_reads = 0
        self.advance_on_head_read = None
        self.advance_at_patch = False
        self.change_release_on_read = None
        self.timeout_write = None
        self.apply_before_timeout = False
        self.symlink_readme = False
        self.pending = {}

    @property
    def writes(self):
        return [(method, endpoint, data) for method, endpoint, data in self.calls if method != "GET"]

    def __call__(self, args, **options):
        if not isinstance(args, list) or args[:4] != ["gh", "api", "--hostname", "github.com"]:
            raise AssertionError(f"Expected a gh argv list targeting github.com: {args!r}")
        if options.get("shell") or options.get("check") is not False or options.get("timeout") != 90:
            raise AssertionError("Unexpected subprocess options")
        method = args[args.index("--method") + 1]
        endpoint = args[6].removeprefix(f"repos/{SYNC.REPOSITORY}").lstrip("/")
        data = json.loads(options["input"]) if options.get("input") is not None else None
        self.calls.append((method, endpoint, data))
        if method == "GET" and endpoint == "":
            result = {"default_branch": self.branch, "private": False}
        elif method == "GET" and endpoint.startswith("git/ref/heads/"):
            self.head_reads += 1
            if self.head_reads == self.advance_on_head_read:
                self.head = self.competing
            result = {"ref": "refs/heads/" + self.branch, "object": {"sha": self.head, "type": "commit"}}
        elif method == "GET" and endpoint.startswith("releases?"):
            self.release_reads += 1
            if self.release_reads == self.change_release_on_read:
                self.releases[1]["body"] += "\nConcurrent change.\n"
            if "--paginate" not in args or "--slurp" not in args:
                raise AssertionError("Release reads must include all pages")
            result = [self.releases[:1], self.releases[1:]]
        elif method == "GET" and endpoint.startswith("git/commits/"):
            result = {"tree": {"sha": self.base_tree}}
        elif method == "GET" and endpoint.startswith("git/trees/"):
            result = {"truncated": False, "tree": [
                {"path": path, "type": "blob", "mode": "120000" if self.symlink_readme else "100644",
                 "sha": SYNC.git_blob_sha(content)}
                for path, content in self.documents.items()]}
        elif method == "GET" and endpoint.startswith("git/blobs/"):
            sha = endpoint.rsplit("/", 1)[-1]
            content = next(content for content in self.documents.values()
                           if SYNC.git_blob_sha(content) == sha)
            result = {"encoding": "base64", "content": base64.b64encode(content.encode()).decode()}
        elif method == "POST" and endpoint == "git/trees":
            if self.timeout_write == "tree":
                raise subprocess.TimeoutExpired("gh", 90)
            if data["base_tree"] != self.base_tree:
                raise AssertionError("Wrong base tree")
            self.pending = {entry["path"]: entry.get("content") for entry in data["tree"]}
            result = {"sha": self.new_tree}
        elif method == "POST" and endpoint == "git/commits":
            if data["parents"] != [self.base] or data["tree"] != self.new_tree:
                raise AssertionError("Commit must have the observed head as its only parent")
            result = {"sha": self.commit}
        elif method == "PATCH" and endpoint.startswith("git/refs/heads/"):
            if data != {"sha": self.commit, "force": False}:
                raise AssertionError("Expected one non-force ref update")
            if self.advance_at_patch:
                self.head = self.competing
            if self.timeout_write == "patch" and not self.apply_before_timeout:
                raise subprocess.TimeoutExpired("gh", 90)
            if self.head != self.base:
                return subprocess.CompletedProcess(args, 1, "", "non-fast-forward")
            self.head = self.commit
            for changed, content in self.pending.items():
                if content is None:
                    self.documents.pop(changed)
                else:
                    self.documents[changed] = content
            if self.timeout_write == "patch":
                raise subprocess.TimeoutExpired("gh", 90)
            result = {"object": {"sha": self.head}}
        else:
            raise AssertionError(f"Unexpected gh API operation: {method} {endpoint}")
        return subprocess.CompletedProcess(args, 0, json.dumps(result), "")


class ReleaseNotesSyncTests(unittest.TestCase):
    def setUp(self):
        temporary_root = SYNC.WORKSPACE / ".tmp"
        temporary_root.mkdir(exist_ok=True)
        self.directory = tempfile.TemporaryDirectory(prefix="release-notes-unit-", dir=temporary_root)
        self.addCleanup(self.directory.cleanup)
        self.output = Path(self.directory.name) / "result"
        self.fake = FakeGh()
        self.mock = patch.object(SYNC.subprocess, "run", self.fake)
        self.mock.start()
        self.addCleanup(self.mock.stop)
        self.docs = Path(self.directory.name) / "docs-user"
        self.docs.mkdir()
        (self.docs / "README.md").write_text("# Docs\n\n[Guide](guide.md#start)\n", encoding="utf-8", newline="")
        (self.docs / "guide.md").write_text("# Guide\r\n\n## start\n[Back](README.md)\n", encoding="utf-8", newline="")
        docs_patch = patch.object(SYNC, "DOCS_SOURCE", self.docs)
        docs_patch.start()
        self.addCleanup(docs_patch.stop)

    def sync(self, *, apply=False, tag="rust-v0.31.13", directory=None, local_checkout=None,
             source_in_destination=False):
        return SYNC.sync(tag, directory or self.output, apply=apply, local_checkout=local_checkout,
                         source_in_destination=source_in_destination)

    def receipt(self):
        return json.loads((self.output / "receipt.json").read_text(encoding="utf-8"))

    def test_dry_run_writes_only_local_candidates_and_handles_historical_x64(self):
        original = self.fake.documents["README.md"]
        result = self.sync()
        self.assertEqual("dry-run", result["status"])
        self.assertEqual([], self.fake.writes)
        expected = {"README.md", "CHANGELOG.md", "release-notes/rust-v0.31.12.md", "release-notes/rust-v0.31.13.md",
                    "docs/README.md", "docs/guide.md"}
        self.assertEqual(expected, set(result["changedPaths"]))
        readme = (self.output / "candidate/README.md").read_text(encoding="utf-8")
        self.assertTrue(readme.startswith(original))
        self.assertIn("eaclip-0.31.13-windows-arm64.zip", readme)
        self.assertEqual(self.fake.releases[1]["body"],
                         (self.output / "candidate/release-notes/rust-v0.31.13.md").read_text(encoding="utf-8"))
        changelog = (self.output / "candidate/CHANGELOG.md").read_text(encoding="utf-8")
        old_line = next(line for line in changelog.splitlines() if "0.31.12" in line)
        self.assertIn("Windows x64", old_line)
        self.assertNotIn("arm64", old_line)
        self.assertNotIn("0.31.14", changelog)

    def test_apply_commits_four_documents_in_one_tree_and_one_non_force_update(self):
        result = self.sync(apply=True)
        self.assertEqual("applied", result["status"])
        self.assertEqual([("POST", "git/trees"), ("POST", "git/commits"), ("PATCH", "git/refs/heads/main")],
                         [(method, endpoint) for method, endpoint, _ in self.fake.writes])
        tree = self.fake.writes[0][2]["tree"]
        self.assertEqual(6, len(tree))
        self.assertEqual(set(result["changedPaths"]), {entry["path"] for entry in tree})
        self.assertEqual(self.fake.commit, self.fake.head)
        self.assertEqual(self.fake.commit, self.receipt()["observedHead"])

    def test_reapplying_unchanged_content_performs_no_remote_writes(self):
        self.sync(apply=True)
        self.fake.calls.clear()
        result = self.sync(apply=True, directory=Path(self.directory.name) / "again")
        self.assertEqual("unchanged", result["status"])
        self.assertEqual([], self.fake.writes)

    def test_branch_advance_before_ref_update_rejects_stale_commit(self):
        self.fake.advance_on_head_read = 3
        with self.assertRaisesRegex(SYNC.SyncError, "branch advanced"):
            self.sync(apply=True)
        self.assertEqual(["POST", "POST"], [method for method, _, _ in self.fake.writes])
        self.assertFalse(self.receipt()["refUpdateAttempted"])
        self.assertEqual(self.fake.competing, self.fake.head)
        self.assertNotIn("CHANGELOG.md", self.fake.documents)

    def test_branch_advance_before_first_write_performs_no_remote_writes(self):
        self.fake.advance_on_head_read = 2
        with self.assertRaisesRegex(SYNC.SyncError, "branch advanced"):
            self.sync(apply=True)
        self.assertEqual([], self.fake.writes)

    def test_race_during_ref_update_is_rejected_without_force_or_retry(self):
        self.fake.advance_at_patch = True
        with self.assertRaisesRegex(SYNC.SyncError, "inspect remote state"):
            self.sync(apply=True)
        self.assertEqual(1, sum(method == "PATCH" for method, _, _ in self.fake.writes))
        self.assertEqual(self.fake.competing, self.receipt()["observedHead"])
        self.assertNotIn("CHANGELOG.md", self.fake.documents)

    def test_concurrent_release_body_change_rejects_plan(self):
        self.fake.change_release_on_read = 2
        with self.assertRaisesRegex(SYNC.SyncError, "Release inputs changed"):
            self.sync(apply=True)
        self.assertEqual([], self.fake.writes)

    def test_selected_draft_empty_and_invalid_tag_are_rejected(self):
        for index, (tag, body) in enumerate((("rust-v0.31.14", None), ("rust-v0.31.13", ""),
                                            ("rust-v0.31.13/../../README", None))):
            with self.subTest(tag=tag, body=body):
                self.fake.releases[1]["body"] = body
                with self.assertRaises(SYNC.SyncError):
                    self.sync(tag=tag, apply=True, directory=Path(self.directory.name) / str(index))
        self.assertEqual([], self.fake.writes)

    def test_readme_marker_replacement_preserves_prefix_and_suffix(self):
        prefix, suffix = "# eaclip\r\n\r\nKeep this introduction.\r\n\r\n", "\r\n\r\n## Other\r\nKeep this too.\r\n"
        self.fake.documents["README.md"] = prefix + SYNC.START + "\r\nOld block.\r\n" + SYNC.END + suffix
        self.sync()
        candidate = (self.output / "candidate/README.md").read_bytes().decode()
        self.assertTrue(candidate.startswith(prefix))
        self.assertTrue(candidate.endswith(suffix))
        self.assertEqual(1, candidate.count(SYNC.START))
        self.assertNotIn("Old block", candidate)

    def test_duplicate_markers_and_symlink_readme_rejected(self):
        self.fake.documents["README.md"] += SYNC.START + SYNC.START
        with self.assertRaisesRegex(SYNC.SyncError, "markers"):
            self.sync(apply=True)
        self.fake.documents["README.md"] = "# eaclip\n"
        self.fake.symlink_readme = True
        with self.assertRaisesRegex(SYNC.SyncError, "regular"):
            self.sync(apply=True, directory=Path(self.directory.name) / "symlink")
        self.assertEqual([], self.fake.writes)

    def test_unknown_tree_result_is_not_retried_and_never_updates_ref(self):
        self.fake.timeout_write = "tree"
        with self.assertRaisesRegex(SYNC.SyncError, "known result"):
            self.sync(apply=True)
        self.assertEqual([("POST", "git/trees")], [(method, endpoint) for method, endpoint, _ in self.fake.writes])
        self.assertEqual("creating-tree", self.receipt()["failedPhase"])

    def test_unknown_ref_result_is_not_retried(self):
        self.fake.timeout_write = "patch"
        with self.assertRaisesRegex(SYNC.SyncError, "known result"):
            self.sync(apply=True)
        self.assertEqual(1, sum(method == "PATCH" for method, _, _ in self.fake.writes))
        self.assertEqual(self.fake.base, self.receipt()["observedHead"])
        self.assertEqual("ref-result-unknown", self.receipt()["failedPhase"])

    def test_timed_out_ref_write_can_be_confirmed_with_one_read(self):
        self.fake.timeout_write = "patch"
        self.fake.apply_before_timeout = True
        result = self.sync(apply=True)
        self.assertEqual("applied-confirmed-after-error", result["status"])
        self.assertEqual(1, sum(method == "PATCH" for method, _, _ in self.fake.writes))
        self.assertEqual(self.fake.commit, result["observedHead"])

    def test_output_outside_repo_temp_is_rejected_before_api_calls(self):
        with self.assertRaisesRegex(SYNC.SyncError, "inside this repository"):
            self.sync(directory=SYNC.WORKSPACE / "unsafe-output")
        self.assertEqual([], self.fake.calls)

    def test_docs_are_mirrored_with_stale_docs_deleted_and_readme_link_once(self):
        self.fake.documents.update({"docs/README.md": "old\n", "docs/removed.md": "gone\n",
                                    "docs-other/keep.md": "outside docs\n"})
        result = self.sync(apply=True)
        self.assertEqual(["docs/removed.md"], result["deletedPaths"])
        deletions = [entry for entry in self.fake.writes[0][2]["tree"] if entry.get("sha", "") is None]
        self.assertEqual(["docs/removed.md"], [entry["path"] for entry in deletions])
        self.assertNotIn("docs/removed.md", self.fake.documents)
        self.assertIn("docs-other/keep.md", self.fake.documents)
        self.assertEqual("# Guide\n\n## start\n[Back](README.md)\n", self.fake.documents["docs/guide.md"])
        readme = self.fake.documents["README.md"]
        self.assertEqual(1, readme.count(SYNC.DOCS_LINK_TARGET))
        self.assertLess(readme.index(SYNC.DOCS_LINK_TARGET), readme.index(SYNC.START))
        self.fake.calls.clear()
        again = self.sync(apply=True, directory=Path(self.directory.name) / "again")
        self.assertEqual("unchanged", again["status"])
        self.assertEqual([], self.fake.writes)

    def test_docs_link_is_added_after_existing_bullets(self):
        original = "# eaclip\n\n- [版本](https://x)\n- [反馈](https://y)\n\n" + SYNC.START + "\nx\n" + SYNC.END + "\n"
        updated = SYNC.ensure_docs_link(original)
        self.assertIn("- [反馈](https://y)\n" + SYNC.DOCS_LINK_LINE + "\n\n" + SYNC.START, updated)
        self.assertEqual(updated, SYNC.ensure_docs_link(updated))
        inside_only = "# eaclip\n\n" + SYNC.START + "\n[x](docs/README.md)\n" + SYNC.END + "\n"
        self.assertEqual(1, SYNC.ensure_docs_link(inside_only).split(SYNC.START)[0].count(SYNC.DOCS_LINK_TARGET))

    def test_same_repository_sync_preserves_source_and_still_updates_public_docs(self):
        source = {"docs/user/README.md": "# Source input\r\n", "docs/user/guide.md": "# Source guide\n"}
        self.fake.documents.update(source | {"docs/README.md": "old\n", "docs/stale.md": "removed\n"})
        result = self.sync(apply=True, source_in_destination=True)
        self.assertTrue(result["sourceInDestination"])
        self.assertEqual(["docs/stale.md"], result["deletedPaths"])
        for path, content in source.items():
            self.assertEqual(content, self.fake.documents[path])
            self.assertNotIn(path, self.fake.pending)
        self.assertNotIn("docs/stale.md", self.fake.documents)
        self.assertEqual((self.docs / "README.md").read_text(encoding="utf-8"), self.fake.documents["docs/README.md"])
        self.fake.calls.clear()
        again = self.sync(apply=True, source_in_destination=True, directory=Path(self.directory.name) / "again")
        self.assertEqual("unchanged", again["status"])
        self.assertEqual([], self.fake.writes)

    def test_cross_repository_default_keeps_exact_mirror_deletion_behavior(self):
        self.fake.documents["docs/user/stale.md"] = "old destination-only document\n"
        result = self.sync(apply=True)
        self.assertFalse(result["sourceInDestination"])
        self.assertEqual(["docs/user/stale.md"], result["deletedPaths"])
        self.assertNotIn("docs/user/stale.md", self.fake.documents)

    def test_same_repository_rejects_mirror_writes_into_source_before_network_access(self):
        (self.docs / "user").mkdir()
        (self.docs / "user/README.md").write_text("# Would replace source\n", encoding="utf-8")
        with self.assertRaisesRegex(SYNC.SyncError, "protected docs/user"):
            self.sync(apply=True, source_in_destination=True)
        self.assertEqual([], self.fake.calls)

    def test_private_or_internal_docs_content_is_rejected_before_api_calls(self):
        for index, text in enumerate((
                "[src](https://github.com/ceastld/CeaQuickerTools/blob/main/x)",
                "see host-tauri/scripts/x",
                "defaults in global_hotkey.rs",
                "D:\\source\\repos",
                "token ghp_" + "a" * 30,
                "[up](../README.md)",
                "[missing](nope.md)")):
            with self.subTest(text=text):
                (self.docs / "guide.md").write_text(text + "\n", encoding="utf-8", newline="")
                with self.assertRaises(SYNC.SyncError):
                    self.sync(apply=True, directory=Path(self.directory.name) / f"bad-{index}")
        self.assertEqual([], self.fake.calls)

    def test_non_markdown_docs_are_rejected(self):
        (self.docs / "image.png").write_bytes(b"\x89PNG")
        with self.assertRaisesRegex(SYNC.SyncError, "Only Markdown"):
            SYNC.public_docs()

    def test_local_checkout_trial_writes_working_tree_only_and_deletes_only_docs(self):
        clone = Path(tempfile.mkdtemp(prefix="eaclip-clone-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(clone, ignore_errors=True))
        (clone / ".git").mkdir()
        (clone / "README.md").write_text("# eaclip\r\n\r\n- [下载](https://x)\r\n", encoding="utf-8", newline="")
        (clone / "docs").mkdir()
        (clone / "docs/stale.md").write_text("stale\n", encoding="utf-8", newline="")
        (clone / "keep.md").write_text("keep\n", encoding="utf-8", newline="")
        self.fake.releases[1]["body"] = "# ClipHost 0.31.13\r\n\r\n- CRLF body from GitHub.\r\n"
        result = self.sync(apply=True, local_checkout=clone)
        self.assertEqual("applied-local", result["status"])
        self.assertEqual([], self.fake.writes)
        self.assertFalse((clone / "docs/stale.md").exists())
        self.assertTrue((clone / "keep.md").exists())
        self.assertTrue((clone / "docs/guide.md").exists())
        self.assertIn(SYNC.DOCS_LINK_LINE, (clone / "README.md").read_text(encoding="utf-8"))
        again = self.sync(apply=True, local_checkout=clone, directory=Path(self.directory.name) / "local-again")
        self.assertEqual("unchanged", again["status"])
        with self.assertRaisesRegex(SYNC.SyncError, "outside this repository"):
            self.sync(local_checkout=SYNC.WORKSPACE, directory=Path(self.directory.name) / "local-bad")


if __name__ == "__main__":
    unittest.main()
