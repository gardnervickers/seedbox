"""Portable changed-path planner. No host probes or execution."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

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
    pattern = re.escape(glob.lstrip('/')).replace(r'\*\*', '.*').replace(r'\*', '[^/]*')
    return re.fullmatch(pattern, path) is not None


def portable(path):
    return (path.endswith('.md') or path.startswith(('docs/', 'harness/', 'tools/mac-tcg/',
            'python/tests/', 'crates/seedbox/tests/', 'crates/seedbox/templates/',
            'crates/seedbox/src/cli/help.rs')) or path.endswith('.py'))


def select_checks(changes, mapping=None, extra=(), diffs=None):
    """Union every matched row and reviewer addition; never subtract a check."""
    mapping = mapping or load_map()
    diffs = diffs or {}
    reasons, skipped, warnings = {}, [], []
    def add(check, reason):
        if reason not in reasons.setdefault(check, []):
            reasons[check].append(reason)
    for revision, paths in changes.items():
        for path in sorted(set(paths)):
            reason = dict(revision=revision, path=path, category='portable')
            if path.endswith('.md') or path.startswith('docs/'):
                skipped.append(reason)
                continue
            if path.startswith('kernel/'):
                warnings.append(f'kernel campaign needed: {path}')
                continue
            code = diffs.get((revision, path), '')
            if path.startswith('crates/seedbox/src/cli/') and help_only(code):
                skipped.append(dict(reason, category='CLI help text'))
                continue
            matched = False
            for rule in mapping['rules']:
                if any(matches(path, glob) for glob in rule['paths']):
                    if 'diff_regex' in rule and not re.search(rule['diff_regex'], code):
                        continue
                    matched = True
                    for check in rule['checks']:
                        add(check, dict(reason, category=rule['category']))
            if not matched:
                if portable(path):
                    skipped.append(reason)
                elif path.startswith('crates/'):
                    for check in mapping['core']:
                        add(check, dict(reason, category='unmatched crates (fail safe)'))
                else:
                    skipped.append(reason)
    for check in extra:
        if check not in mapping['checks']:
            raise ValueError('unknown extra check: ' + check)
        reason = dict(revision='reviewer', path='--extra ' + check, category='reviewer addition')
        add(check, reason)
        if check in ('core', 'perf-replay'):
            for required in mapping['core']:
                add(required, dict(reason, category='core dependency'))
    return dict(checks={check: reasons[check] for check in mapping['checks'] if check in reasons},
                skipped=skipped, warnings=warnings)


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.PIPE)


def plan_revision(repo, base, head, extra=()):
    base_sha = git(repo, 'rev-parse', '--verify', '--end-of-options', base + '^{commit}').decode().strip()
    tip = git(repo, 'rev-parse', '--verify', '--end-of-options', head + '^{commit}').decode().strip()
    raw = git(repo, 'diff', '--name-only', '--no-renames', '-z', base_sha + '...' + tip, '--')
    paths = [path.decode('utf-8', 'surrogateescape') for path in raw.split(b'\0') if path]
    changes = {tip: paths}
    diffs = {}
    mapping = load_map()
    for path in paths:
        if path.startswith('crates/seedbox/src/cli/') or any(
                'diff_regex' in rule and any(matches(path, glob) for glob in rule['paths'])
                for rule in mapping['rules']):
            raw_diff = git(repo, 'diff', '--no-ext-diff', '--no-textconv', '--no-color', '--unified=0',
                           base_sha + '...' + tip, '--', path).decode('utf-8', 'surrogateescape')
            diffs[tip, path] = changed_code(raw_diff)
    return dict(select_checks(changes, mapping, extra=extra, diffs=diffs), base=base_sha, target=tip, changes=changes)


def changed_code(diff):
    return '\n'.join(line for line in diff.splitlines()
                     if line.startswith(('+', '-')) and not line.startswith(('+++', '---')))


def help_only(code):
    """Only changed comment/help strings; executable CLI changes fail safe."""
    lines = [line[1:].strip() for line in code.splitlines() if line[1:].strip()]
    return bool(lines) and all(line.startswith('//') or re.fullmatch(
        r'(?:#\[(?:arg|command)\()?\s*(?:about|long_about|help|long_help)\s*=\s*"(?:[^"\\]|\\.)*"\s*(?:,|\)\])?',
        line) for line in lines)


def format_plan(plan, mapping=None):
    mapping = mapping or load_map()
    lines = [f"target: {plan['target']}", f"base: {plan['base']}"]
    for check, reasons in plan['checks'].items():
        spec = mapping['checks'][check]
        detail = spec.get('description', spec.get('project', 'harness/worlds/' + spec.get('world', '') + '.json'))
        lines.append(f'check {check}: {detail}')
        for reason in reasons:
            lines.append(f"  reason: {reason['path']} ({reason['category']})")
    lines.extend(plan['warnings'])
    if not plan['checks']:
        lines.append('host: none')
    return '\n'.join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('base', metavar='BASE')
    parser.add_argument('head', metavar='HEAD')
    parser.add_argument('--json', action='store_true', help='Print the plan as JSON.')
    parser.add_argument('--extra', action='append', default=[], metavar='CHECK', help='Add a check (repeatable).')
    args = parser.parse_args(argv)
    try:
        plan = plan_revision(Path.cwd(), args.base, args.head, args.extra)
        print(json.dumps(plan, indent=2) if args.json else format_plan(plan))
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print('error: ' + str(error), file=sys.stderr)
        return 2
