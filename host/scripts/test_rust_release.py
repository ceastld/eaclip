"""Offline frozen-plan and scope contracts. No account, network or publication writes."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

SPEC = importlib.util.spec_from_file_location('rust_release', Path(__file__).with_name('rust_release.py'))
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class PlanTests(unittest.TestCase):
    def setUp(self):
        scratch = release.ROOT / '.tmp' / 'rust-release-tests'
        scratch.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'clip'
        (self.root / 'host').mkdir(parents=True)
        self.profile = release.read_json(release.ROOT / release.PROFILE)
        self.profile['actionRegistry'] = '../workspace/registry.yaml'
        (self.root / release.PROFILE).write_text(json.dumps(self.profile), encoding='utf-8')
        (self.root / 'host/Cargo.toml').write_text('[workspace.package]\nversion="0.31.16"\n')
        (self.root / 'host/launcher-compatibility.json').write_text(json.dumps({
            'requiredLauncherVersion': '0.31.7.0', 'arm64RequiredLauncherVersion': '0.31.14.1'}))
        notes = self.root / 'release-notes/public/rust-v0.31.16.md'
        notes.parent.mkdir(parents=True)
        notes.write_text('# ClipHost 0.31.16\n\n- Reviewed change.\n', encoding='utf-8')
        self.registry = self.root.parent / 'workspace/registry.yaml'
        self.registry.parent.mkdir()
        self.registry.write_text('''clip-main:
  clipboard-n10:
    title: Clipboard
    sharedActionId: df6cc8fc-1a67-4576-fcb0-08deace01986
    actionId: 32c12786-9bb8-4b0c-8d55-7e6a4c8a5d10
  super-menu-n10:
    title: Super menu
    sharedActionId: 2e6361fc-2d6c-442e-e199-08def9e1fa34
    actionId: 217847a2-2a0c-4396-9e14-61f4742ee16b
''', encoding='utf-8')
        self.run_git('init', '-q')
        self.run_git('add', '.')
        self.run_git('-c', 'user.name=Release test', '-c', 'user.email=release-test@example.invalid',
                     'commit', '-qm', 'fixture')
        self.run_git('tag', 'rust-v0.31.16')

    def run_git(self, *args):
        subprocess.run(['git', '-C', str(self.root), *args], capture_output=True, check=True)

    def plan(self, **args):
        return release.make_plan(root=self.root, **args)

    def test_plan_freezes_tag_notes_and_both_action_identities(self):
        plan, path = self.plan()
        self.assertTrue(plan['sourceTagMatchedAtPlan'])
        self.assertEqual('ceastld/eaclip', plan['profile']['sourceRepository'])
        self.assertEqual(['x64'], [a['name'] for a in plan['profile']['architectures']])
        self.assertEqual(['clip-main/clipboard-n10', 'clip-main/super-menu-n10'],
                         [a['key'] for a in plan['actions']])
        self.assertEqual('menu', plan['actions'][1]['command'])
        self.assertEqual('0.31.14.1', plan['actions'][0]['minimumLauncherVersion'])
        self.assertEqual(plan, release.load_plan(path, self.root)[0])

    def test_plan_freezes_selected_public_tag_instead_of_development_head(self):
        (self.root / 'private-development.txt').write_text('not part of the public source snapshot')
        self.run_git('add', 'private-development.txt')
        self.run_git('-c', 'user.name=Release test', '-c', 'user.email=release-test@example.invalid',
                     'commit', '-qm', 'private development after public tag')
        plan, _ = self.plan(source_ref='rust-v0.31.16')
        self.assertEqual(release.git(self.root, 'rev-parse', 'rust-v0.31.16^{commit}'), plan['sourceCommit'])
        self.assertNotEqual(release.git(self.root, 'rev-parse', 'HEAD'), plan['sourceCommit'])
        self.assertTrue(plan['sourceTagMatchedAtPlan'])

    def test_legacy_private_source_plan_remains_readable(self):
        self.profile['sourceRepository'] = 'ceastld/CeaQuickerTools'
        (self.root / release.PROFILE).write_text(json.dumps(self.profile), encoding='utf-8')
        plan, path = self.plan()
        self.assertEqual('ceastld/CeaQuickerTools', release.load_plan(path, self.root)[0]['profile']['sourceRepository'])
        # Updating the current profile does not rewrite or invalidate a frozen plan.
        self.profile['sourceRepository'] = 'ceastld/eaclip'
        (self.root / release.PROFILE).write_text(json.dumps(self.profile), encoding='utf-8')
        self.assertEqual(plan, release.load_plan(path, self.root)[0])

    def test_selected_old_tag_uses_legacy_paths_and_keeps_frozen_plan_readable(self):
        self.run_git('mv', 'host', 'host-tauri')
        legacy = dict(self.profile, versionSource='host-tauri/Cargo.toml',
                      launcherCompatibilityFile='host-tauri/launcher-compatibility.json')
        (self.root / 'host-tauri/release-profile.json').write_text(json.dumps(legacy), encoding='utf-8')
        self.run_git('add', '.')
        self.run_git('-c', 'user.name=Release test', '-c', 'user.email=release-test@example.invalid',
                     'commit', '-qm', 'legacy layout')
        self.run_git('tag', 'legacy-layout')
        self.run_git('mv', 'host-tauri', 'host')
        (self.root / release.PROFILE).write_text(json.dumps(self.profile), encoding='utf-8')
        self.run_git('add', '.')
        self.run_git('-c', 'user.name=Release test', '-c', 'user.email=release-test@example.invalid',
                     'commit', '-qm', 'current layout')

        plan, path = self.plan(source_ref='legacy-layout')
        self.assertEqual('host-tauri/Cargo.toml', plan['profile']['versionSource'])
        self.assertEqual('host-tauri/launcher-compatibility.json', plan['profile']['launcherCompatibilityFile'])
        self.assertEqual(release.git(self.root, 'rev-parse', 'legacy-layout^{commit}'), plan['sourceCommit'])
        self.assertEqual('0.31.16', plan['version'])
        self.assertEqual('0.31.14.1', plan['launcherCompatibility']['arm64RequiredLauncherVersion'])
        frozen = path.read_bytes()
        # A frozen historical plan never depends on today's profile or layout.
        (self.root / release.PROFILE).write_text('{}', encoding='utf-8')
        self.assertEqual(plan, release.load_plan(path, self.root)[0])
        self.assertEqual(frozen, path.read_bytes())

    def test_selected_commit_rejects_both_workspace_layouts(self):
        legacy = self.root / 'host-tauri'
        legacy.mkdir()
        (legacy / 'Cargo.toml').write_text('[workspace.package]\nversion="0.31.16"\n')
        self.run_git('add', '.')
        self.run_git('-c', 'user.name=Release test', '-c', 'user.email=release-test@example.invalid',
                     'commit', '-qm', 'ambiguous layout')
        with self.assertRaisesRegex(release.ReleaseError, 'exactly one host or host-tauri'):
            self.plan()
        self.assertFalse((self.root / '.tmp').exists())

    def test_dirty_legacy_directory_does_not_change_selected_commit_layout(self):
        legacy = self.root / 'host-tauri'
        legacy.mkdir()
        (legacy / 'Cargo.toml').write_text('[workspace.package]\nversion="9.9.9"\n')
        plan, _ = self.plan()
        self.assertEqual('host/Cargo.toml', plan['profile']['versionSource'])
        self.assertEqual('host/launcher-compatibility.json', plan['profile']['launcherCompatibilityFile'])
        self.assertEqual('0.31.16', plan['version'])

    def test_managed_worktree_uses_primary_checkout_sibling_registry(self):
        worktree = self.root / '.tmp' / 'managed' / 'clip'
        worktree.parent.mkdir(parents=True)
        self.run_git('worktree', 'add', '--detach', str(worktree), 'HEAD')
        self.addCleanup(lambda: self.run_git('worktree', 'remove', '--force', str(worktree)))
        plan, _ = release.make_plan(root=worktree)
        self.assertEqual('menu', plan['actions'][1]['command'])
        self.assertEqual('217847a2-2a0c-4396-9e14-61f4742ee16b', plan['actions'][1]['actionId'])

    def test_explicit_architectures_keep_native_package_identities(self):
        arm64 = dict(name='arm64', target='aarch64-pc-windows-msvc',
                     packageName='cea.cliphost.rust.arm64')
        self.profile['architectures'].append(arm64)
        (self.root / release.PROFILE).write_text(json.dumps(self.profile), encoding='utf-8')
        plan, _ = self.plan()
        self.assertEqual(['x64', 'arm64'], [a['name'] for a in plan['profile']['architectures']])

    def test_architecture_scope_rejects_empty_duplicate_or_mismatched_packages(self):
        for architectures in ([], self.profile['architectures'] * 2,
                              [dict(name='x64', target='aarch64-pc-windows-msvc',
                                    packageName='cea.cliphost.rust')]):
            profile = copy.deepcopy(self.profile)
            profile['architectures'] = architectures
            (self.root / release.PROFILE).write_text(json.dumps(profile), encoding='utf-8')
            with self.subTest(architectures=architectures), self.assertRaises(release.ReleaseError):
                self.plan()

    def test_asset_checks_use_only_the_frozen_architectures_locally_and_remotely(self):
        import rust_release_assets as assets
        definitions = {
            'x64': dict(name='x64', target='x86_64-pc-windows-msvc', packageName='cea.cliphost.rust'),
            'arm64': dict(name='arm64', target='aarch64-pc-windows-msvc', packageName='cea.cliphost.rust.arm64'),
        }
        for selected in (('x64',), ('arm64',), ('x64', 'arm64')):
            label = '-'.join(selected)
            self.profile['architectures'] = [definitions[a] for a in selected]
            (self.root / release.PROFILE).write_text(json.dumps(self.profile), encoding='utf-8')
            _, plan_path = self.plan(output_directory=self.root / '.tmp' / ('plan-' + label))
            directory = self.root / '.tmp' / ('assets-' + label)
            directory.mkdir()
            for arch in selected:
                stem = f'eaclip-0.31.16-windows-{arch}'
                with zipfile.ZipFile(directory / (stem + '.zip'), 'w') as archive:
                    archive.writestr('ClipHost-Rust/cliphost-app.exe', b'frozen fixture bytes')
                (directory / (stem + '.build-info.json')).write_text(json.dumps(dict(
                    product='ClipHost Rust', version='0.31.16.0', architecture=arch,
                    target=definitions[arch]['target'], renderer='gpui', profile='release',
                    embeddedFrontend=False)), encoding='utf-8')
                (directory / (stem + '.SHA256SUMS.txt')).write_text(''.join(
                    hashlib.sha256((directory / name).read_bytes()).hexdigest() + '  ' + name + '\n'
                    for name in (stem + '.zip', stem + '.build-info.json')), encoding='utf-8')
            manifest = assets.validate_local_assets('0.31.16', directory, architectures=selected)
            metadata = dict(tag_name=manifest['tag'], draft=False,
                html_url='https://github.com/ceastld/eaclip/releases/tag/' + manifest['tag'],
                assets=[dict(name=f['name'], state='uploaded', size=f['sizeBytes'],
                    digest='sha256:' + f['sha256'], browser_download_url=
                    'https://github.com/ceastld/eaclip/releases/download/' + manifest['tag'] + '/' + f['name'])
                    for f in manifest['files']])
            def download(asset, destination):
                destination.write_bytes((directory / asset['name']).read_bytes())
            with patch.object(assets, '_run_gh', return_value=json.dumps(metadata)), \
                    patch.object(assets, '_download_public_asset', side_effect=download):
                for remote in (False, True):
                    with self.subTest(architectures=selected, remote=remote):
                        result = release.verify_assets(plan_path, directory, remote=remote, root=self.root)
                        self.assertEqual('verified', result['status'])
                        verified = release.read_json(result['receipt'])['result']
                        self.assertEqual({f['name'] for f in manifest['files']},
                                         {f['name'] for f in verified['assets' if remote else 'files']})

    def test_selected_commit_not_dirty_worktree_controls_version_and_notes(self):
        (self.root / 'host/Cargo.toml').write_text('[workspace.package]\nversion="9.9.9"\n')
        plan, _ = self.plan()
        self.assertEqual('0.31.16', plan['version'])
        with self.assertRaises(release.ReleaseError):
            self.plan(version='9.9.9')

    def test_changed_registry_does_not_retarget_existing_plan(self):
        plan, path = self.plan()
        self.registry.write_text('not a registry', encoding='utf-8')
        self.assertEqual(plan['actions'], release.load_plan(path, self.root)[0]['actions'])

    def test_existing_plan_is_never_overwritten(self):
        _, path = self.plan()
        original = path.read_bytes()
        with self.assertRaises(FileExistsError):
            self.plan()
        self.assertEqual(original, path.read_bytes())

    def test_tampered_plan_or_notes_refuse_resume(self):
        plan, path = self.plan()
        plan['actions'][0]['actionId'] = plan['actions'][1]['actionId']
        path.write_text(json.dumps(plan), encoding='utf-8')
        with self.assertRaises(release.ReleaseError):
            release.load_plan(path, self.root)
        plan, path = self.plan(output_directory=self.root / '.tmp/other')
        (path.parent / 'public-notes.md').write_text('different notes')
        with self.assertRaises(release.ReleaseError):
            release.load_plan(path, self.root)

    def test_dependency_scope_has_no_action_or_github_write_stage(self):
        plan, path = self.plan(scope='dependencies')
        self.assertEqual([], plan['actions'])
        self.assertEqual('dependencies', release.status(path, root=self.root)['scope'])
        with self.assertRaises(release.ReleaseError):
            self.plan(scope='dependencies', action_keys=['clip-main/super-menu-n10'])

    def test_scope_cannot_select_legacy_or_csharp_panel(self):
        for keys in (['clip-main/panel-n10'], ['clip-legacy/super-menu'],
                     ['clip-main/clipboard-n10'] * 2, []):
            with self.subTest(keys=keys), self.assertRaises(release.ReleaseError):
                self.plan(action_keys=keys)

    def test_invalid_or_duplicate_registry_identity_is_rejected(self):
        original = self.registry.read_text(encoding='utf-8')
        for text in (original.replace('217847a2-2a0c-4396-9e14-61f4742ee16b', 'not-a-guid'),
                     original.replace('217847a2-2a0c-4396-9e14-61f4742ee16b',
                                      '32c12786-9bb8-4b0c-8d55-7e6a4c8a5d10'),
                     original + '  super-menu-n10:\n    title: Duplicate\n'):
            self.registry.write_text(text, encoding='utf-8')
            with self.assertRaises(release.ReleaseError):
                self.plan()

    def test_profile_cannot_disable_safety_gate_or_change_release_repository(self):
        for key, value in (('gateSet', 'none'), ('releaseRepository', 'other/repo'),
                           ('publicNotesPattern', 'release-notes/v{version}.md'),
                           ('tagPattern', 'v{version}'),
                           ('defaultActions', [dict(key='clip-main/panel-n10', command='panel show')])):
            profile = copy.deepcopy(self.profile)
            profile[key] = value
            (self.root / release.PROFILE).write_text(json.dumps(profile), encoding='utf-8')
            with self.assertRaises(release.ReleaseError):
                self.plan()

    def test_outputs_stay_in_project_scratch(self):
        with self.assertRaises(release.ReleaseError):
            self.plan(output_directory=self.root.parent / 'elsewhere')

    def test_status_marks_changed_evidence_stale_and_never_claims_full_completion(self):
        plan, path = self.plan()
        evidence = path.parent / 'capture.json'
        evidence.write_text('{"ok":true}')
        directory = release.check_directory(path, 'actions')
        release.write_json(directory / 'result.json', dict(
            planId=plan['planId'], status='verified', kind='actions', inputs=release.input_stamp([evidence])))
        value = release.status(path, root=self.root)
        self.assertEqual('verified', value['checks'][0]['status'])
        self.assertEqual('incomplete', value['status'])
        self.assertFalse(value['toolPerformedPublication'])
        evidence.write_text('different')
        self.assertEqual('stale', release.status(path, root=self.root)['checks'][0]['status'])

    def test_foreign_receipt_is_rejected(self):
        _, path = self.plan()
        directory = release.check_directory(path, 'actions')
        release.write_json(directory / 'result.json', dict(planId='other', status='verified'))
        with self.assertRaises(release.ReleaseError):
            release.status(path, root=self.root)

    def test_status_detects_a_deleted_source_tag(self):
        _, path = self.plan()
        self.run_git('tag', '-d', 'rust-v0.31.16')
        value = release.status(path, root=self.root)
        self.assertTrue(value['sourceTagMatchedAtPlan'])
        self.assertFalse(value['sourceTagMatches'])

    def test_status_is_compact_and_preserves_failed_attempt_history(self):
        plan, path = self.plan()
        for state in ('verified', 'failed'):
            directory = release.check_directory(path, 'actions')
            release.write_json(directory / 'result.json', dict(
                planId=plan['planId'], status=state, kind='actions', inputs=[]))
        checks = release.status(path, root=self.root)['checks']
        self.assertEqual(1, len(checks))
        self.assertEqual(2, checks[0]['attempts'])
        self.assertEqual('failed', checks[0]['status'])

    def test_one_action_failure_is_a_failed_bundle(self):
        plan, path = self.plan(scope='actions')
        evidence = path.parent / 'evidence'
        item = evidence / 'clipboard-n10'
        item.mkdir(parents=True)
        (item / 'preview.json').write_text('{}')
        (item / 'owner.json').write_text('{}')
        def verifier(action, version, directory):
            if action['key'].endswith('super-menu-n10'):
                raise ValueError('wrong version')
            return {'status': 'verified'}
        with patch.dict('sys.modules', {'rust_release_actions': type('Fake', (), {
                'verify_action_evidence': staticmethod(verifier)})}):
            result = release.verify_actions(path, evidence, root=self.root)
        self.assertEqual('failed', result['status'])
        self.assertEqual(['verified', 'failed'], [a['status'] for a in result['actions']])

    def test_shared_changelog_is_part_of_stale_evidence_detection(self):
        _, path = self.plan(scope='actions', action_keys=['clip-main/super-menu-n10'])
        evidence = path.parent / 'evidence'
        item = evidence / 'super-menu-n10'
        item.mkdir(parents=True)
        for name in ('preview.json', 'owner.json'):
            (item / name).write_text('{}')
        changelog = evidence / 'changelog.txt'
        changelog.write_text('Reviewed notes')
        with patch.dict('sys.modules', {'rust_release_actions': type('Fake', (), {
                'verify_action_evidence': staticmethod(lambda *args: {'status': 'verified'})})}):
            self.assertEqual('verified', release.verify_actions(path, evidence, root=self.root)['status'])
        changelog.write_text('Changed notes')
        self.assertEqual('stale', release.status(path, root=self.root)['checks'][0]['status'])

    def test_evidence_changed_during_verification_fails_the_attempt(self):
        _, path = self.plan(scope='actions', action_keys=['clip-main/super-menu-n10'])
        evidence = path.parent / 'evidence'
        item = evidence / 'super-menu-n10'
        item.mkdir(parents=True)
        for name in ('preview.json', 'owner.json'):
            (item / name).write_text('{}')
        def verifier(*args):
            (item / 'preview.json').write_text('Changed during check')
            return {'status': 'verified'}
        with patch.dict('sys.modules', {'rust_release_actions': type('Fake', (), {
                'verify_action_evidence': staticmethod(verifier)})}):
            self.assertEqual('failed', release.verify_actions(path, evidence, root=self.root)['status'])


if __name__ == '__main__':
    unittest.main()
