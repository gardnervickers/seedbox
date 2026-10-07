"""Portable path map and merge-base planner. No host probes or execution."""
import json
from pathlib import Path
import re
import subprocess

MAP = Path(__file__).with_name('host-check-map.json')


def load_map(path=MAP):
    mapping = json.loads(path.read_text())
    if mapping['version'] != 1:
        raise ValueError('unsupported host check map version')
    known = set(mapping['checks'])
    for checks in [mapping['core'], *(rule['checks'] for rule in mapping['rules'])]:
        if not set(checks) <= known:
            raise ValueError('map references an unknown check')
    for rule in mapping['rules']:
        if 'diff_regex' in rule:
            re.compile(rule['diff_regex'])
    return mapping


def matches(path, glob):
    # * is one path component; ** is recursive. A leading / anchors the root.
    pattern = re.escape(glob.lstrip('/')).replace(r'\*\*', '.*').replace(r'\*', '[^/]*')
    return re.fullmatch(pattern, path) is not None


def select_checks(changes, mapping=None, diffs=None):
    """changes maps revision labels to paths; preserve every matching reason."""
    mapping = mapping or load_map()
    diffs = diffs or {}
    reasons, skipped, warnings = {}, [], []
    for revision, paths in changes.items():
        for path in sorted(set(paths)):
            matched = False
            engine = any(path.startswith(root) for root in mapping['engine_roots'])
            if engine:
                for check in mapping['core']:
                    reasons.setdefault(check, []).append(dict(revision=revision, path=path, category='engine-core'))
            for rule in mapping['rules']:
                globs = [glob for glob in rule['paths'] if matches(path, glob)]
                if not globs:
                    continue
                if 'diff_regex' in rule and not re.search(rule['diff_regex'], diffs.get((revision, path), '')):
                    continue
                matched = True
                reason = dict(revision=revision, path=path, category=rule['category'], globs=globs)
                for check in rule['checks']:
                    if reason not in reasons.setdefault(check, []):
                        reasons[check].append(reason)
                if not rule['checks'] and not engine:
                    skipped.append(reason)
            if not matched:
                if engine:
                    warnings.append(f'{revision}: unmapped engine path {path}; selected core')
                else:
                    # Unknown runtime/build inputs must not silently disappear.
                    warnings.append(f'{revision}: unmapped path {path}; selected core for review')
                    for check in mapping['core']:
                        reasons.setdefault(check, []).append(dict(revision=revision, path=path, category='unmapped'))
    return dict(checks={check: reasons[check] for check in mapping['checks'] if check in reasons},
                skipped=skipped, warnings=warnings)


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.PIPE)


def plan_revisions(repo, revisions, base='origin/main'):
    """Union merge-base diffs; execute once at the last revision (the train tip)."""
    changes, merges, diffs = {}, {}, {}
    mapping = load_map()
    base_sha = git(repo, 'rev-parse', '--verify', '--end-of-options', base + '^{commit}').decode().strip()
    tips = []
    for revision in revisions:
        tip = git(repo, 'rev-parse', '--verify', '--end-of-options', revision + '^{commit}').decode().strip()
        merge = git(repo, 'merge-base', base_sha, tip).decode().strip()
        # Disable rename detection: both the deleted and added paths select checks.
        raw = git(repo, 'diff', '--name-only', '--no-renames', '-z', merge, tip, '--')
        changes[tip] = [path.decode('utf-8', 'surrogateescape') for path in raw.split(b'\0') if path]
        for path in changes[tip]:
            if any('diff_regex' in rule and any(matches(path, glob) for glob in rule['paths'])
                   for rule in mapping['rules']):
                diff = git(repo, 'diff', '--no-ext-diff', '--no-textconv', '--no-color', '--unified=0', merge, tip, '--', path).decode('utf-8', 'surrogateescape')
                diffs[tip, path] = changed_code(diff)
        merges[tip] = merge
        tips.append(tip)
    if not tips:
        raise ValueError('at least one revision is required')
    return dict(select_checks(changes, mapping, diffs), base=base_sha, merge_bases=merges, revisions=tips,
                target=tips[-1], changes=changes)


def changed_code(diff):
    """Changed lines and hunk function labels, excluding unrelated context."""
    return '\n'.join(line for line in diff.splitlines()
                     if (line.startswith(('+', '-')) and not line.startswith(('+++', '---')))
                     or line.startswith('@@'))


def format_plan(plan, mapping=None):
    mapping = mapping or load_map()
    lines = [f"target: {plan['target']}", f"base: {plan['base']}"]
    for check, reasons in plan['checks'].items():
        spec = mapping['checks'][check]
        detail = spec.get('description', spec.get('project', 'harness/worlds/' + spec.get('world', '') + '.json'))
        lines.append(f'check {check}: {detail}')
        for reason in reasons:
            lines.append(f"  reason: {reason['revision'][:12]} {reason['path']} ({reason['category']})")
    for reason in plan['skipped']:
        lines.append(f"skip: {reason['revision'][:12]} {reason['path']} ({reason['category']}; no host run)")
    lines.extend('warning: ' + warning for warning in plan['warnings'])
    if not plan['checks']:
        lines.append('host: no checks selected')
    return '\n'.join(lines)
