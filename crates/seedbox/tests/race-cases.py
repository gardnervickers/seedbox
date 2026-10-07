"""Focused public-CLI qualification of Seedbox's own race workload."""
import copy
import json
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys


class CommandFailure(Exception):
    def __init__(self, status, name):
        self.status = status
        super().__init__(f'{name}: CLI/program exited {status}; evidence retained')


class Unresolved(Exception):
    pass


def evidence(record, raw, console, seed):
    if record['identity']['seed'] != seed or raw.get('seed') != seed:
        raise ValueError('wrong seed')
    if record.get('outcome') != 'PASS' or (record.get('report') or {}).get('reason') != 'COMPLETED':
        raise Unresolved('race did not complete successfully')
    if record['report'].get('workload_exit') != {'service': 'n1', 'code': 0}:
        raise ValueError('missing successful race exit')
    inputs = record['identity']['inputs']
    if set(inputs['guests']) != {'n1'} or inputs['placement'] != {'n1': 'n1'}:
        raise ValueError('unexpected race placement')
    if raw.get('outcome') != 'end_machine_exited':
        raise Unresolved('missing terminal VMM outcome')
    machines = raw.get('machines', [])
    if len(machines) != 1 or machines[0].get('name') != 'n1' or machines[0].get('state') != 'Exited(0)':
        raise ValueError('missing or duplicate race machine')
    def digest(value):
        if not isinstance(value, str) or not re.fullmatch('[0-9a-f]{64}', value):
            raise ValueError('missing execution digest')
        return value
    world = digest(raw.get('world_witness'))
    if record['report'].get('witness') != world:
        raise ValueError('contradictory world witness')
    machine = digest(machines[0].get('witness'))
    state = digest(machines[0].get('state_digest'))
    if record['report'].get('full_state') != {'n1': state}:
        raise ValueError('missing complete full-state evidence')
    found = re.findall(r'^RACE mode=(\w+) counter=(-?\d+) expected=(-?\d+) lost=(-?\d+)\s*$', console, re.M)
    if len(found) != 2 or [row[0] for row in found] != ['racy', 'atomic']:
        raise ValueError('missing or duplicate race output')
    for mode, counter, expected, lost in found:
        counter, expected, lost = map(int, (counter, expected, lost))
        if expected <= 0 or counter < 0 or lost < 0 or expected - counter != lost:
            raise ValueError('invalid race arithmetic')
        if mode == 'atomic' and lost:
            raise ValueError('atomic control lost updates')
    return {'loss': int(found[0][3]), 'witnesses': (world, machine, state)}


def same_inputs(reference, record):
    a, b = copy.deepcopy(reference['identity']), copy.deepcopy(record['identity'])
    a.pop('seed'); b.pop('seed')
    if a != b:
        raise Unresolved('prepared inputs or semantic configuration drifted')


def diverse(results):
    losses = [r['loss'] for r in results]
    if not any(loss > 0 for loss in losses) or len(set(losses)) < 2:
        raise Unresolved(f'proposed seeds did not demonstrate race diversity: {losses}')
    return next(i for i, loss in enumerate(losses) if loss > 0)


def reproduced(original, repeated, record):
    replay = record.get('replay') or {}
    if replay.get('status') != 'MATCH' or not all(replay.get(k) is True for k in
            ('outcome_matches', 'witness_matches', 'full_state_matches', 'fault_history_matches')):
        raise ValueError('replay did not MATCH complete evidence')
    if original != repeated:
        raise ValueError('race result or execution evidence changed on replay')


def main():
    cli, vmm, kernel, cpu, output_arg = sys.argv[1:]
    output = Path(output_arg).resolve()
    repo = Path(__file__).resolve().parents[3]
    state = output / 'state'
    for source, name in ((cli, 'seedbox'), (vmm, 'seedbox-vmm'), (kernel, 'bzImage')):
        shutil.copyfile(source, output / name)
    (output / 'seedbox').chmod(0o755)
    cli, vmm, kernel = map(str, (output / 'seedbox', output / 'seedbox-vmm', output / 'bzImage'))
    active = None
    def interrupted(signum, _frame):
        if active is not None:
            active.send_signal(signum)
        else:
            raise SystemExit(3)
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, interrupted)
    def command(args, name):
        nonlocal active
        with (output / name).open('w') as log:
            active = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
            try:
                status = active.wait()
            finally:
                active = None
        if status:
            raise CommandFailure(status, name)
        return (output / name).read_text()
    # Only this campaign's finite inputs are copied; no caller checkout is mutated.
    context = output / 'guest'
    context.mkdir()
    shutil.copyfile(repo / 'guest/race.c', context / 'race.c')
    shutil.copyfile(repo / 'harness/acceptance/driver/race.py', output / 'race.py')
    dockerfile = (repo / 'guest/acceptance-race.Dockerfile').read_text()
    for index, base in enumerate(('docker.io/library/alpine:3.23', 'docker.io/library/python:3.13-alpine')):
        command(['podman', 'pull', '--platform', 'linux/amd64', base], f'base-{index}-pull.log')
        inspect = command(['podman', 'image', 'inspect', base], f'base-{index}.json')
        refs = json.loads(inspect)[0]['RepoDigests']
        pin = next(ref for ref in refs if '@sha256:' in ref)
        dockerfile = dockerfile.replace(base, pin)
    (context / 'acceptance-race.Dockerfile').write_text(dockerfile)
    yaml = (repo / 'harness/acceptance/workloads/race.yaml').read_text()
    yaml = yaml.replace('../../../guest', './guest').replace('../driver/race.py', './race.py')
    compose = output / 'compose.yaml'
    compose.write_text(yaml)
    common = ['--cpu', cpu, '--state', str(state)]
    runtime = ['--vmm', vmm, '--kernel', kernel, '--guest-init', str(output / 'compose-init'),
               '--busybox', __import__('os').environ['SEEDBOX_BUSYBOX']]
    before = command(['python3', str(repo / 'harness/msr54.py'), 'show',
                      ','.join(map(str, sorted(__import__('batch').siblings(int(cpu)))))], 'msr-before.txt')
    attempts, records, results = [], [], []
    try:
        for seed in (1, 2, 3, 4):
            text = command([cli, 'run', str(compose), '--seed', str(seed), '--full-state', *common, *runtime], f'seed-{seed}.log')
            paths = re.findall(r'^[A-Z]*: (.+)$', text, re.M)
            if len(paths) != 1:
                raise ValueError('missing unique run path')
            attempt = Path(paths[0])
            record = json.loads(command([cli, 'inspect', str(attempt), '--json'], f'seed-{seed}.json'))
            if records:
                same_inputs(records[0], record)
            result = evidence(record, json.loads((attempt / 'output/vmm.json').read_text()),
                              (attempt / 'output/machines/n1.console').read_text(), seed)
            attempts.append(attempt); records.append(record); results.append(result)
        selected = diverse(results)
        text = command([cli, 'replay', str(attempts[selected]), *common], 'replay.log')
        paths = re.findall(r'^[A-Z]*: (.+)$', text, re.M)
        if len(paths) != 1:
            raise ValueError('missing unique replay path')
        repeated_path = Path(paths[0])
        record = json.loads(command([cli, 'inspect', str(repeated_path), '--json'], 'replay.json'))
        same_inputs(records[selected], record)
        repeated = evidence(record, json.loads((repeated_path / 'output/vmm.json').read_text()),
                            (repeated_path / 'output/machines/n1.console').read_text(), selected + 1)
        if record.get('replay_of') != records[selected]['attempt']:
            raise ValueError('wrong replay baseline')
        reproduced(results[selected], repeated, record)
    finally:
        after = command(['python3', str(repo / 'harness/msr54.py'), 'show',
                         ','.join(map(str, sorted(__import__('batch').siblings(int(cpu)))))], 'msr-after.txt')
        if before != after or list(state.glob('runs/*/output/msr-original.json')):
            raise ValueError('MSR restoration failed or snapshot remains')
    print(f'PASS: race losses {[r["loss"] for r in results]}; seed {selected + 1} replay MATCH with world, machine and full state')


if __name__ == '__main__':
    sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'harness'))
    try:
        main()
    except CommandFailure as error:
        print(f'error: {error}', file=sys.stderr)
        raise SystemExit(error.status if error.status > 0 else 3)
    except Unresolved as error:
        print(f'unresolved: {error}', file=sys.stderr)
        raise SystemExit(3)
    except (OSError, ValueError, KeyError, StopIteration) as error:
        print(f'error: {error}', file=sys.stderr)
        raise SystemExit(2)
