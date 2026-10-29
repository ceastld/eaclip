"""Frozen Rust/n10 release plans and reusable read-only checks. No publish side effects.

Only Python's standard library is needed. Normal stdout is one compact JSON record;
large evidence stays in the selected plan directory, never in an agent response.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import uuid

ROOT = Path(__file__).resolve().parents[2]
PROFILE = 'host/release-profile.json'
RUST_ACTIONS = {'clip-main/clipboard-n10': 'clip toggle', 'clip-main/super-menu-n10': 'menu'}
GATES = ('source-and-version', 'executable-identity', 'payload-and-zip-sha256',
         'native-runtime', 'original-zip-antivirus', 'official-launcher-availability')
EXTERNAL = {
    'full': ('source-tests', 'antivirus', 'dependencies', 'release-docs', 'account-sync', 'runtime'),
    'actions': ('official-dependencies', 'account-sync', 'runtime'),
    'dependencies': ('antivirus', 'official-launcher-availability', 'dependency-distribution'),
}


class ReleaseError(ValueError):
    pass


def digest_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def digest_json(value) -> str:
    return digest_bytes(json.dumps(value, ensure_ascii=False, sort_keys=True,
                                  separators=(',', ':')).encode('utf-8'))


def digest_file(path: Path) -> str:
    with available(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def available(path: Path) -> Path:
    """Inspect explicitly named ancestors before opening a possible cloud placeholder."""
    path = Path(os.path.abspath(path))
    if path.drive.startswith('\\\\') or any(re.match(r'(?i)^(onedrive.*|dropbox|icloud.*)$', p)
                                                for p in path.parts):
        raise ReleaseError(f'Cloud/network paths are not release inputs: {path}')
    for item in (*reversed(path.parents), path):
        if item.exists() or item.is_symlink():
            attributes = getattr(item.lstat(), 'st_file_attributes', 0)
            if item.is_symlink() or attributes & 0x441400:
                raise ReleaseError(f'Unavailable, placeholder or linked release path: {item}')
    return path


def read_json(path):
    return json.loads(available(Path(path)).read_text(encoding='utf-8-sig'))


def write_json(path: Path, value):
    with available(path).open('x', encoding='utf-8', newline='\n') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write('\n')


def git(root: Path, *arguments, optional=False):
    result = subprocess.run(['git', '-C', str(root), *arguments], capture_output=True,
                            text=True, encoding='utf-8', errors='strict', shell=False)
    if result.returncode:
        if optional:
            return None
        raise ReleaseError('Cannot read the selected Git source: ' + result.stderr.strip())
    return result.stdout.rstrip('\n')


def registry_entries(path: Path):
    """The workspace's documented flat YAML subset; reject ambiguous identities."""
    entries, section, key = {}, '', ''
    for line in available(path).read_text(encoding='utf-8-sig').splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        if match := re.fullmatch(r'([\w-]+):\s*', line):
            section, key = match[1], ''
        elif match := re.fullmatch(r'  ([\w/-]+):\s*', line):
            key = section + '/' + match[1]
            if key in entries:
                raise ReleaseError(f'Duplicate registry key: {key}')
            entries[key] = {}
        elif match := re.fullmatch(r'    (\w+):\s*(.*?)\s*', line):
            if not key or match[1] in entries[key]:
                raise ReleaseError('Ambiguous publish registry field.')
            entries[key][match[1]] = match[2].strip('"\'')
        else:
            raise ReleaseError('Unsupported publish registry syntax; use the flat YAML subset.')
    return entries


def load_profile(root=ROOT):
    profile = read_json(root / PROFILE)
    if (profile.get('schemaVersion') != 1 or profile.get('product') != 'rust-gpui'
            or profile.get('renderer') != 'gpui' or profile.get('gateSet') != 'rust-gpui-v1'
            or profile.get('sourceRepository') not in ('ceastld/eaclip', 'ceastld/CeaQuickerTools')
            or profile.get('releaseRepository') != 'ceastld/eaclip'
            or profile.get('workflow') != 'rust-release.yml'):
        raise ReleaseError('Unsupported Rust release profile or publication target.')
    supported = {'x64': ('x86_64-pc-windows-msvc', 'cea.cliphost.rust'),
                 'arm64': ('aarch64-pc-windows-msvc', 'cea.cliphost.rust.arm64')}
    architectures = profile.get('architectures', [])
    if (not architectures or len({a.get('name') for a in architectures}) != len(architectures)
            or any((a.get('target'), a.get('packageName')) != supported.get(a.get('name'))
                   for a in architectures)):
        raise ReleaseError('The Rust release profile requires unique supported native architectures.')
    if profile['publicNotesPattern'] != 'release-notes/public/rust-v{version}.md':
        raise ReleaseError('Rust releases require the explicit public Rust notes pattern.')
    if (profile['tagPattern'] != 'rust-v{version}' or profile['versionSource'] != 'host/Cargo.toml'
            or profile['launcherCompatibilityFile'] != 'host/launcher-compatibility.json'
            or profile['defaultActions'] != [dict(key=key, command=value) for key, value in RUST_ACTIONS.items()]):
        raise ReleaseError('Rust source, tag and default action scope must match the supported product contract.')
    return profile


def inside_scratch(root: Path, path: Path):
    path = available(path)
    if not path.resolve().is_relative_to((root / '.tmp').resolve()):
        raise ReleaseError('Plan and check outputs must stay under this repository .tmp/.')
    return path


def source_profile(root: Path, commit: str, profile: dict):
    """Freeze paths from the selected Git tree, including pre-rename releases."""
    layouts = git(root, 'ls-tree', '-d', '--name-only', commit, '--',
                  'host', 'host-tauri').splitlines()
    if len(layouts) != 1 or layouts[0] not in ('host', 'host-tauri'):
        raise ReleaseError('Selected source must contain exactly one host or host-tauri workspace.')
    return dict(profile, versionSource=layouts[0] + '/Cargo.toml',
                launcherCompatibilityFile=layouts[0] + '/launcher-compatibility.json')


def make_plan(*, root=ROOT, scope='full', source_ref='HEAD', version=None, notes_file=None,
              output_directory=None, action_keys=None):
    root = Path(root).resolve()
    if scope not in EXTERNAL:
        raise ReleaseError('Unsupported release scope.')
    profile = load_profile(root)
    commit = git(root, 'rev-parse', '--verify', source_ref + '^{commit}')
    profile = source_profile(root, commit, profile)
    manifest = tomllib.loads(git(root, 'show', commit + ':' + profile['versionSource']))
    source_version = manifest['workspace']['package']['version']
    if not re.fullmatch(r'\d+\.\d+\.\d+', source_version) or version and version != source_version:
        raise ReleaseError('Version must match the selected source commit, without an automatic bump.')
    version = source_version
    notes_path = profile['publicNotesPattern'].format(version=version)
    if notes_file:
        named_notes = available(Path(notes_file))
        notes = named_notes.read_text(encoding='utf-8-sig')
        notes_origin = {'kind': 'explicit-file', 'path': str(named_notes)}
    else:
        notes = git(root, 'show', commit + ':' + notes_path) + '\n'
        notes_origin = {'kind': 'git', 'path': notes_path, 'commit': commit}
    heading = next((line for line in notes.splitlines() if line.startswith('#')), '')
    if (not notes.strip() or not re.search(r'(?<![\d.])' + re.escape(version) + r'(?![\d.])', heading)
            or not re.search(r'^[-*]\s+\S', notes, re.M)):
        raise ReleaseError('Public notes need the selected version heading and reviewed bullet points.')
    compat = json.loads(git(root, 'show', commit + ':' + profile['launcherCompatibilityFile']))
    for value in compat.values():
        if not isinstance(value, str) or not re.fullmatch(r'\d+\.\d+\.\d+\.\d+', value):
            raise ReleaseError('Invalid frozen launcher compatibility version.')
    definitions = {item['key']: item for item in profile['defaultActions']}
    if len(definitions) != len(profile['defaultActions']):
        raise ReleaseError('Duplicate default action key.')
    selected = list(action_keys) if action_keys is not None else list(definitions)
    if len(selected) != len(set(selected)) or set(selected) - definitions.keys():
        raise ReleaseError('Action selection contains duplicates or an action outside the Rust profile.')
    if scope == 'dependencies':
        if action_keys:
            raise ReleaseError('Dependency-only plans cannot publish actions.')
        selected = []
    elif not selected:
        raise ReleaseError('Action/full plans require an explicit nonempty action scope.')
    registry = {}
    if selected:
        registry_path = root / profile['actionRegistry']
        if not registry_path.exists():
            # A managed checkout shares Git objects with the primary checkout;
            # its sibling workspace remains beside that primary repository.
            common = Path(git(root, 'rev-parse', '--path-format=absolute', '--git-common-dir'))
            registry_path = common.parent / profile['actionRegistry']
        registry = registry_entries(registry_path)
    actions = []
    for key in selected:
        if key not in registry:
            raise ReleaseError(f'Missing registered Rust action: {key}')
        entry = registry[key]
        for field in ('actionId', 'sharedActionId'):
            try:
                if str(uuid.UUID(entry[field])) != entry[field].lower():
                    raise ValueError()
            except (ValueError, KeyError) as error:
                raise ReleaseError(f'Invalid {field} for {key}') from error
        actions.append(dict(key=key, title=entry['title'], actionId=entry['actionId'],
                            sharedActionId=entry['sharedActionId'], command=definitions[key]['command'],
                            minimumLauncherVersion=max(compat.values(), key=lambda v: tuple(map(int, v.split('.'))))))
    for field in ('actionId', 'sharedActionId'):
        if len({item[field] for item in actions}) != len(actions):
            raise ReleaseError('Two release actions point to the same identity.')
    tag = profile['tagPattern'].format(version=version)
    tag_commit = git(root, 'rev-parse', '--verify', 'refs/tags/' + tag + '^{commit}', optional=True)
    plan = dict(schemaVersion=1, product='rust-gpui', scope=scope, repoRoot=str(root),
                version=version, hostVersion=version + '.0', sourceCommit=commit, tag=tag,
                tagCommitAtPlan=tag_commit, sourceTagMatchedAtPlan=tag_commit == commit,
                profile=profile, profileSha256=digest_json(profile),
                launcherCompatibility=compat, actions=actions,
                notes=dict(file='public-notes.md', origin=notes_origin,
                           sha256=digest_bytes(notes.encode('utf-8'))),
                gates=list(GATES), externalStages=list(EXTERNAL[scope]))
    plan['planId'] = digest_json(plan)
    directory = inside_scratch(root, Path(output_directory) if output_directory else
                               root / profile['stateRoot'] / (version + '-' + scope))
    directory.mkdir(parents=True, exist_ok=False)
    (directory / 'public-notes.md').write_text(notes, encoding='utf-8', newline='\n')
    write_json(directory / 'plan.json', plan)
    return plan, directory / 'plan.json'


def load_plan(path, root=ROOT):
    path = inside_scratch(Path(root).resolve(), Path(path))
    plan = read_json(path)
    frozen = dict(plan)
    plan_id = frozen.pop('planId', None)
    if (plan.get('schemaVersion') != 1 or plan.get('product') != 'rust-gpui'
            or plan_id != digest_json(frozen) or plan.get('profileSha256') != digest_json(plan['profile'])
            or Path(plan['repoRoot']).resolve() != Path(root).resolve()
            or plan.get('gates') != list(GATES)):
        raise ReleaseError('Frozen plan identity or content changed. Create a new plan after review.')
    if plan['notes']['file'] != 'public-notes.md':
        raise ReleaseError('Unexpected frozen notes path.')
    notes = available(path.parent / plan['notes']['file']).read_bytes()
    if digest_bytes(notes) != plan['notes']['sha256']:
        raise ReleaseError('Frozen public notes changed. Do not resume with different release inputs.')
    return plan, path


def input_stamp(paths):
    return [dict(path=str(available(Path(path))), sha256=digest_file(Path(path)))
            for path in paths]


def check_directory(plan_path, kind):
    directory = plan_path.parent / 'checks' / kind / (datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')
                                                    + '-' + uuid.uuid4().hex[:8])
    available(directory).mkdir(parents=True, exist_ok=False)
    return directory


def verify_assets(plan_path, asset_directory, *, remote=False, root=ROOT):
    from rust_release_assets import validate_local_assets, verify_remote_release
    plan, plan_path = load_plan(plan_path, root)
    if plan['scope'] != 'full':
        raise ReleaseError('Portable GitHub asset verification belongs to full plans; dependency ZIPs use their own frozen receipts.')
    directory = check_directory(plan_path, 'remote-assets' if remote else 'local-assets')
    try:
        assets = available(Path(asset_directory))
        names = [f'eaclip-{plan["version"]}-windows-{a["name"]}.{suffix}'
                 for a in plan['profile']['architectures']
                 for suffix in ('zip', 'build-info.json', 'SHA256SUMS.txt')]
        inputs = input_stamp([assets / name for name in names])
        architectures = tuple(a['name'] for a in plan['profile']['architectures'])
        if remote:
            result = verify_remote_release(plan['version'], assets,
                                           repository=plan['profile']['releaseRepository'],
                                           expected_draft=False, output_directory=directory / 'remote',
                                           architectures=architectures)
        else:
            result = validate_local_assets(plan['version'], assets, architectures=architectures)
        if input_stamp([assets / name for name in names]) != inputs:
            raise ReleaseError('Frozen assets changed while the check was running.')
        record = dict(schemaVersion=1, planId=plan['planId'], status='verified', inputs=inputs,
                      kind='remote-assets' if remote else 'local-assets', result=result)
        write_json(directory / 'result.json', record)
    except Exception as error:
        write_json(directory / 'result.json', dict(planId=plan['planId'], status='failed',
                                                  kind='remote-assets' if remote else 'local-assets',
                                                  error=str(error)))
        raise ReleaseError(f'Asset check failed; evidence: {directory / "result.json"}: {error}') from error
    return dict(status='verified', kind=record['kind'], receipt=str(directory / 'result.json'))


def verify_actions(plan_path, evidence_directory, *, root=ROOT):
    from rust_release_actions import verify_action_evidence
    plan, plan_path = load_plan(plan_path, root)
    if not plan['actions']:
        raise ReleaseError('This plan has no action publication scope.')
    evidence = available(Path(evidence_directory))
    directory = check_directory(plan_path, 'actions')
    results, inputs = [], []
    for action in plan['actions']:
        try:
            item = available(evidence / action['key'].rsplit('/', 1)[-1])
            paths = [item / name for name in ('preview.json', 'owner.json')]
            for name in ('before-owner.json', 'changelog.txt'):
                if (item / name).exists():
                    paths.append(item / name)
            if (evidence / 'changelog.txt').exists():
                paths.append(evidence / 'changelog.txt')
            before_inputs = input_stamp(paths)
            result = verify_action_evidence(action, plan['version'], evidence)
            if input_stamp(paths) != before_inputs:
                raise ReleaseError('Frozen action evidence changed while the check was running.')
            inputs.extend(before_inputs)
            results.append(dict(key=action['key'], status='verified', result=result))
        except (ValueError, OSError, KeyError) as error:
            results.append(dict(key=action['key'], status='failed', error=str(error)))
    passed = all(item['status'] == 'verified' for item in results)
    record = dict(schemaVersion=1, planId=plan['planId'], status='verified' if passed else 'failed',
                  kind='actions', inputs=inputs, results=results)
    write_json(directory / 'result.json', record)
    return dict(status=record['status'], kind='actions',
                receipt=str(directory / 'result.json'),
                actions=[dict(key=item['key'], status=item['status']) for item in results])


def status(plan_path, *, root=ROOT):
    plan, plan_path = load_plan(plan_path, root)
    checks = {}
    # Only this plan's explicit check directory; never scan account/config folders.
    base = plan_path.parent / 'checks'
    if base.exists():
        available(base)
        for kind in sorted(base.iterdir()):
            available(kind)
            if not kind.is_dir():
                continue
            for attempt in sorted(kind.iterdir()):
                available(attempt)
                path = attempt / 'result.json'
                if not path.exists():
                    continue
                record = read_json(path)
                if record.get('planId') != plan['planId']:
                    raise ReleaseError('Check receipt belongs to a different frozen plan.')
                state = record.get('status', 'failed')
                if state == 'verified':
                    try:
                        if input_stamp([i['path'] for i in record['inputs']]) != record['inputs']:
                            state = 'stale'
                    except (OSError, ValueError):
                        state = 'stale'
                previous = checks.get(record['kind'], {})
                checks[record['kind']] = dict(kind=record['kind'], status=state, receipt=str(path),
                                             attempts=previous.get('attempts', 0) + 1)
    current_tag_commit = git(Path(root), 'rev-parse', '--verify', 'refs/tags/' + plan['tag'] + '^{commit}', optional=True)
    return dict(status='incomplete', planId=plan['planId'], scope=plan['scope'],
                version=plan['version'], sourceCommit=plan['sourceCommit'],
                sourceTagMatchedAtPlan=plan['sourceTagMatchedAtPlan'], sourceTagCommit=current_tag_commit,
                sourceTagMatches=current_tag_commit == plan['sourceCommit'],
                actions=[a['key'] for a in plan['actions']], checks=list(checks.values()),
                externalStages=[dict(stage=item, status='not-automated') for item in plan['externalStages']],
                toolPerformedPublication=False)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    plan = commands.add_parser('plan', help='Freeze a reviewed Git source, notes and Rust action scope.')
    plan.add_argument('--scope', choices=tuple(EXTERNAL), default='full')
    plan.add_argument('--source-ref', default='HEAD')
    plan.add_argument('--version')
    plan.add_argument('--notes-file')
    plan.add_argument('--output-directory')
    plan.add_argument('--action', action='append')
    for name in ('status', 'verify-assets', 'verify-actions'):
        command = commands.add_parser(name)
        command.add_argument('--plan', required=True)
        if name == 'verify-assets':
            command.add_argument('--asset-directory', required=True)
            command.add_argument('--remote', action='store_true')
        if name == 'verify-actions':
            command.add_argument('--evidence-directory', required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == 'plan':
            value, path = make_plan(scope=args.scope, source_ref=args.source_ref, version=args.version,
                                    notes_file=args.notes_file, output_directory=args.output_directory,
                                    action_keys=args.action)
            result = dict(status='planned', planPath=str(path), planId=value['planId'],
                          version=value['version'], sourceCommit=value['sourceCommit'], scope=value['scope'],
                          sourceTagMatchedAtPlan=value['sourceTagMatchedAtPlan'], actions=[a['key'] for a in value['actions']])
        elif args.command == 'status':
            result = status(args.plan)
        elif args.command == 'verify-assets':
            result = verify_assets(args.plan, args.asset_directory, remote=args.remote)
        else:
            result = verify_actions(args.plan, args.evidence_directory)
        print(json.dumps(result, ensure_ascii=False, separators=(',', ':')))
        return 1 if result['status'] == 'failed' else 0
    except (ValueError, OSError, KeyError, TypeError) as error:
        print(json.dumps(dict(status='failed', error=str(error)), ensure_ascii=False, separators=(',', ':')))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
