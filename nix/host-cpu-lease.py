#!/usr/bin/env python3
"""Installed root boundary. Never run this copy directly from a PR checkout."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def owned_input(path):
    path = Path(path).resolve(strict=True)
    if not path.is_file():
        raise ValueError('stored input must be a regular file')
    for entry in (path, *path.parents):
        info = entry.stat()
        if info.st_uid != 0 or info.st_mode & 0o022:
            raise ValueError(f'stored input is not root-owned and immutable to runner: {entry}')
    return path


def job_path(text, roots):
    path = Path(text)
    if not re.fullmatch(r'job-[a-z0-9_]+', path.name):
        raise ValueError('expected a coordinator job directory')
    if path.is_symlink() or path.resolve().parent not in [Path(root).resolve() for root in roots]:
        raise ValueError('job escaped the configured jobs directory')
    return path.resolve()


def work_path(text, job):
    path = Path(text)
    if not re.fullmatch(r'host-check-[0-9a-f]{7}-cpu(?:[1-9]|1[0-5])-[A-Za-z0-9]+', path.name):
        raise ValueError('expected a host-check work directory')
    if path.is_symlink() or path.resolve().parent != job.resolve():
        raise ValueError('work directory escaped the job')
    return path.resolve(strict=True)


def main():
    if os.geteuid() != 0 or len(sys.argv) < 4:
        raise ValueError('requires installed sudo wrapper: CONFIG smoke|clean PATH ...')
    config = json.loads(Path(sys.argv[1]).read_text())
    operation, text = sys.argv[2:4]
    if operation == 'reap' and len(sys.argv) == 4:
        root = Path(text).resolve(strict=True)
        if root not in [Path(p).resolve() for p in config['jobRoots']]:
            raise ValueError('unknown runner jobs directory')
        for path in root.iterdir():
            if path.name.startswith('job-'):
                subprocess.run([config['python'], '-I', __file__, sys.argv[1], 'clean', str(path)], check=True)
        return 0
    if operation == 'clean' and len(sys.argv) == 4:
        job = job_path(text, config['jobRoots'])
    elif operation == 'smoke' and len(sys.argv) == 7:
        job = job_path(str(Path(text).parent), config['jobRoots'])
    else:
        raise ValueError('only smoke WORK CPU INIT dev|pinned, clean JOB and reap RUNNER-JOBS are accepted')
    unit_name = 'seedbox-host-' + job.parent.name + '-' + job.name
    unit = unit_name + '.service'
    if operation == 'clean':
        # systemctl stop waits through the smoke driver's restoration grace and
        # then kills remaining cgroup members. A missing/collected unit is fine.
        state = subprocess.run([config['systemctl'], 'show', unit, '--property=LoadState', '--value'],
                               check=True, capture_output=True, text=True).stdout.strip()
        if state != 'not-found':
            subprocess.run([config['systemctl'], 'stop', unit], check=True)
        if job.exists():
            shutil.rmtree(job)  # Python's fd-based, symlink-resistant implementation.
        return 0
    work = work_path(text, job)
    cpu, init_text, mode = sys.argv[4:7]
    if not re.fullmatch(r'[1-9]|1[0-5]', cpu) or mode not in ('dev', 'pinned'):
        raise ValueError('invalid CPU pair or engine mode')
    for sibling in (int(cpu), int(cpu) + 16):
        actual = Path(f'/sys/devices/system/cpu/cpu{sibling}/topology/thread_siblings_list').read_text().strip()
        if actual != f'{cpu},{int(cpu) + 16}':
            raise ValueError('unexpected SMT topology')
    kernel = owned_input(config['kernel'])
    pinned = owned_input(config['pinnedEngine'])
    src = work / 'src'
    cli = (src / 'target/release/seedbox').resolve(strict=True)
    init = Path(init_text).resolve(strict=True)
    if not cli.is_relative_to(src) or not init.is_relative_to(src / 'target/release/build') or init.name != 'compose-init':
        raise ValueError('CLI or supervisor escaped this worktree')
    engine = pinned if mode == 'pinned' else (src / 'crates/seedbox-vmm/target/release/seedbox-vmm').resolve(strict=True)
    if mode == 'dev' and not engine.is_relative_to(src):
        raise ValueError('development engine escaped this worktree')
    command = [config['python'], '-I', config['smoke'], '--cli', str(cli), '--engine', str(engine),
               '--kernel', str(kernel), '--guest-init', str(init), '--busybox', config['busybox'],
               '--cpu', cpu, '--tool-path', config['toolPath'], '--output', str(work / 'smoke')]
    if mode == 'dev':
        command += ['--allow-dev-engine', '--pinned-engine', str(pinned)]
    # Smoke/owned/fixture source is installed from the reviewed module revision,
    # not imported from the PR. CLI/VMM code is still trusted privileged code.
    # Use a separate unit so cancellation of sudo cannot orphan root processes.
    return subprocess.call([config['systemdRun'], '--quiet', '--wait', '--pipe', '--collect',
        '--unit=' + unit, '--property=Type=exec', '--property=KillMode=control-group',
        '--property=TimeoutStopSec=45s', '--property=RuntimeMaxSec=6min',
        '--property=UMask=0077', '--property=WorkingDirectory=' + str(work),
        '--property=RuntimeDirectory=' + unit_name, '--property=RuntimeDirectoryMode=0700',
        '--setenv=SEEDBOX_HOST_PAIR_GUARDS=' + config['pairGuards'], '--', *command],
        env={'PATH': config['toolPath'], 'LANG': 'C'})


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print('host CPU wrapper: ' + str(error), file=sys.stderr)
        raise SystemExit(2)
