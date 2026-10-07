#!/usr/bin/env python3
"""Focused public run/replay with the smoke's owned cleanup checks."""
import json
import os
import signal
from pathlib import Path
import shlex
import shutil
import sys
import time
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / 'smoke'))
from amd import Campaign, TOTAL_SECONDS, lock_free, pair, phase_walls, save


def run_pair(job, campaign, project, verifier=None):
    """Run a focused workload once, then require its ordinary retained MATCH."""
    a = campaign.args
    state = campaign.state
    flags = ['--allow-dev-engine'] if job.get('pinned_engine') else []
    runtime = ['--kernel', a.kernel, '--vmm', a.engine, '--guest-init', a.guest_init,
               '--busybox', a.busybox, '--cpu', str(a.cpu), '--state', str(state), '--memory-mb', '512']
    expected = job['spec']['expected']
    code, stdout, _ = campaign.invoke('run', [a.cli, 'run', str(project), '--full-state', *runtime, *flags], True)
    if code != (1 if expected == 'FAIL' else 0) or not stdout.startswith(expected + ': '):
        raise RuntimeError('focused run did not reach expected ' + expected)
    baseline_run = campaign.last_run
    baseline = campaign.inspect('run-inspect', baseline_run)
    if baseline['outcome'] != expected or baseline['error'] is not None:
        raise RuntimeError('focused run outcome differs')
    if verifier:
        verifier(baseline_run)
    code, stdout, _ = campaign.invoke('replay', [a.cli, 'replay', str(baseline_run),
                    '--cpu', str(a.cpu), '--state', str(state), *flags], True)
    replay = campaign.inspect('replay-inspect', campaign.last_run)
    if code or replay['replay']['status'] != 'MATCH' or replay['identity'] != baseline['identity']:
        raise RuntimeError('focused default replay did not MATCH')
    if not all(replay['replay'][field] is True for field in
               ('outcome_matches', 'witness_matches', 'full_state_matches', 'fault_history_matches')):
        raise RuntimeError('focused replay comparison incomplete')
    if verifier:
        verifier(campaign.last_run)


def main():
    if sys.platform != 'linux' or os.geteuid() != 0:
        raise RuntimeError('focused host execution requires Linux and root')
    job = json.loads(Path(sys.argv[1]).read_text())
    root = Path(job['output']) / 'compose'
    root.mkdir(mode=0o700)
    (root / 'home').mkdir()
    args = SimpleNamespace(cli=job['cli'], engine=job['engine'], kernel=job['kernel'],
        guest_init=job['guest_init'], busybox=job['busybox'], cpu=job['pair'][0], tool_path=job['tool_path'])
    campaign = Campaign(args, root, time.monotonic() + TOTAL_SECONDS)
    def interrupted(sig, frame):
        raise InterruptedError('focused check interrupted: ' + str(sig))
    old = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        campaign.cpus = pair(args.cpu)
        campaign.locks = lock_free(campaign.cpus)
        project = Path(job['src']) / job['spec']['project']
        verifier = None
        if job['name'] == 'control-cases':
            sys.path.insert(0, str(Path(job['src']) / 'examples/faults'))
            from generate import generate
            from run import control_project
            from verify import verify_control_cases
            projects = generate(root / 'projects')
            control_project(projects)
            project = projects / 'control-cases'
            verifier = verify_control_cases
        run_pair(job, campaign, project, verifier)
        campaign.remaining()
        save(root / 'result.json', {'phase_wall_seconds': phase_walls(root)})
        shutil.rmtree(campaign.state)
        return 0
    except BaseException as error:
        save(root / 'failure.json', {'error': repr(error), 'phase_wall_seconds': phase_walls(root)})
        print('cleanup: sudo rm -rf -- ' + shlex.quote(str(campaign.state)), file=sys.stderr)
        raise
    finally:
        for sig, handler in old.items():
            signal.signal(sig, handler)
        for name, seconds in phase_walls(root).items():
            print(f'phase {name}: {seconds:.1f}s', flush=True)


if __name__ == '__main__':
    raise SystemExit(main())
