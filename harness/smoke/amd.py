"""Finite public-CLI smoke, staged source only until the issue68 runtime gate."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import sys
import time

from owned import run_owned_replay

REPO = Path(__file__).resolve().parents[2]
TOTAL_SECONDS = 300
CLEANUP_SECONDS = 30
MSR = 0xc0011020


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def result_directory(stdout, state, expected=None):
    lines = [line for line in stdout.splitlines() if re.match(r'^[A-Z_]+: ', line)]
    if len(lines) != 1 or not lines[0].startswith('PASS: '):
        raise RuntimeError('missing or ambiguous PASS run directory')
    run = Path(lines[0].split(': ', 1)[1]).resolve(strict=True)
    if run.parent != (state / 'runs').resolve(strict=True):
        raise RuntimeError('result escaped the owned state')
    if expected is not None and run != expected.resolve(strict=True):
        raise RuntimeError('public result does not name the newly owned attempt')
    return run


def cpulist(text):
    result = set()
    for item in text.strip().split(','):
        bounds = list(map(int, item.split('-')))
        result.update(range(bounds[0], bounds[-1] + 1))
    return result


def pair(cpu):
    cpus = cpulist(Path(f'/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list').read_text())
    if len(cpus) != 2 or cpu not in cpus:
        raise RuntimeError('smoke requires one SMT pair')
    online = cpulist(Path('/sys/devices/system/cpu/online').read_text())
    if not cpus <= online or not cpus <= os.sched_getaffinity(0):
        raise RuntimeError('both siblings must be online and allowed')
    for sibling in cpus:
        if cpulist(Path(f'/sys/devices/system/cpu/cpu{sibling}/topology/thread_siblings_list').read_text()) != cpus:
            raise RuntimeError('nonreciprocal sibling topology')
    return sorted(cpus)


def lock_free(cpus, expected=None):
    held, identity = [], {}
    try:
        for cpu in cpus:
            path = Path(f'/tmp/seedbox-msr-cpu-{cpu}.lock')
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            held.append(fd)
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode):
                raise RuntimeError('canonical lease is not regular')
            identity[str(cpu)] = [info.st_dev, info.st_ino]
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if expected is not None and identity != expected:
            raise RuntimeError('canonical lock identity changed')
        return identity
    finally:
        for fd in held:
            os.close(fd)


def pinned_workloads(cpus):
    selected = set(cpus)
    for proc in Path('/proc').iterdir():
        if not proc.name.isdecimal() or int(proc.name) == os.getpid():
            continue
        try:
            for thread in (proc / 'task').iterdir():
                fields = (thread / 'stat').read_text().rsplit(')', 1)[1].split()
                if int(fields[6]) & 0x00200000:  # Authoritative PF_KTHREAD, never a name heuristic.
                    continue
                allowed = os.sched_getaffinity(int(thread.name))
                if allowed and allowed <= selected:
                    raise RuntimeError('userspace workload pinned to the selected pair')
        except (FileNotFoundError, ProcessLookupError):
            continue


def snapshot(cpus):
    values = {}
    for cpu in cpus:
        fd = os.open(f'/dev/cpu/{cpu}/msr', os.O_RDONLY | os.O_CLOEXEC)
        try:
            raw = os.pread(fd, 8, MSR)
            if len(raw) != 8:
                raise RuntimeError('short MSR read')
            values[str(cpu)] = int.from_bytes(raw, 'little')
        finally:
            os.close(fd)
    return values


def closure(record):
    inputs = record['identity']['inputs']
    objects = {inputs['kernel'], inputs['profile']['engine']}
    objects.update(inputs['auxiliary'].values())
    objects.update(inputs['guests'].values())
    for image in inputs['images'].values():
        objects.update([image['manifest'], image['config'], *image['layers']])
    return objects


def verify_qualification(record, dev_engine):
    marker = record['identity']['inputs']['profile'].get('semantic_parameters', {}).get('engine_qualification')
    if marker != ('dev-unpinned' if dev_engine else None):
        raise RuntimeError('engine qualification marker differs from requested smoke mode')
    if record['report'].get('engine_qualification') != ('dev-unpinned' if dev_engine else None):
        raise RuntimeError('dev engine marker missing from run report')


def verify_record(record, run, state):
    if record['outcome'] != 'PASS' or record['error'] is not None or record['allow_engine_change']:
        raise RuntimeError('baseline/replay did not genuinely PASS')
    if record['omitted_logs'] or not record['report']['full_state']:
        raise RuntimeError('smoke requires full state and complete retained logs')
    streams = {name: value for name, value in record['logs'].items()
               if name.endswith(('.console', '.mgmt'))}
    machines = set(record['identity']['inputs']['guests'])
    expected = {f'machines/{name}.{suffix}' for name in machines for suffix in ('console', 'mgmt')}
    if set(streams) != expected:
        raise RuntimeError('missing opaque console/management stream')
    for value in closure(record) | set(record['logs'].values()) | {record['run_id']}:
        if not re.fullmatch('[0-9a-f]{64}', value) or digest(state / 'objects/sha256' / value) != value:
            raise RuntimeError('retained closure/log object missing or corrupt')
    for name, value in record['logs'].items():
        path = Path(name)
        if path.is_absolute() or '..' in path.parts or digest(run / 'output' / path) != value:
            raise RuntimeError('materialized retained log missing or corrupt')
    return streams


def compare(base, replay, baseline_streams, replay_streams):
    checks = replay['replay']
    if replay['identity'] != base['identity'] or replay['run_id'] != base['run_id']:
        raise RuntimeError('captured semantic identity drifted')
    if checks['status'] != 'MATCH' or not all(checks[name] is True for name in
            ('outcome_matches', 'witness_matches', 'full_state_matches', 'fault_history_matches')):
        raise RuntimeError('replay mismatch or incomplete comparison')
    for field in ('reason', 'workload_exit', 'witness', 'full_state', 'faults'):
        if base['report'][field] != replay['report'][field]:
            raise RuntimeError('deterministic report differs: ' + field)
    if baseline_streams != replay_streams:
        raise RuntimeError('opaque guest streams differ')


def verify_performance(base, replay, parameter=None, value=None):
    expected = base['performance_parameters'].copy()
    if parameter is not None:
        if parameter not in expected or expected[parameter] == value:
            raise RuntimeError('baseline cannot exercise the requested performance change')
        expected[parameter] = value
    if replay['performance_parameters'] != expected:
        raise RuntimeError('recorded performance parameters differ from the requested change')


class Campaign:
    def __init__(self, args, root, deadline):
        self.args, self.root, self.deadline = args, root, deadline
        self.state = root / 'state'
        self.env = {'HOME': str(root / 'home'), 'PATH': args.tool_path, 'LANG': 'C'}
        self.cpus = self.locks = None

    def invoke(self, name, command, execution=False):
        self.remaining()
        phase = self.root / name
        phase.mkdir()
        originals = {}
        old = set((self.state / 'runs').iterdir()) if (self.state / 'runs').exists() else set()
        before = snapshot(self.cpus) if execution else None
        if execution:
            lock_free(self.cpus, self.locks)
        def capture():
            if execution and (self.state / 'runs').exists():
                for run in set((self.state / 'runs').iterdir()) - old:
                    source = run / 'output/msr-original.json'
                    if source.exists() and run not in originals:
                        raw = source.read_bytes()
                        value = json.loads(raw)
                        if set(value) != set(before) or any(type(v) is not int or not 0 <= v < 2**64 for v in value.values()):
                            raise RuntimeError('invalid owned MSR original')
                        (phase / ('msrs-' + run.name + '.json')).write_bytes(raw)
                        originals[run] = value
        life = None
        started = time.monotonic()
        save(phase / 'command.json', command)
        try:
            with (phase / 'stdout').open('x') as out, (phase / 'stderr').open('x') as err:
                life = run_owned_replay(command, self.env, out, err,
                    self.deadline - CLEANUP_SECONDS, self.deadline,
                    lambda child: save(phase / 'spawn.json', {'pid': child.pid}),
                    capture, lambda value: save(phase / 'lifecycle.json', value), cwd=self.root)
        finally:
            # Cleanup measurements precede every exit/text assertion, including errors.
            if execution:
                errors = []
                after = free = None
                try:
                    after = snapshot(self.cpus)
                    free = lock_free(self.cpus, self.locks)
                except Exception as error:
                    errors.append(repr(error))
                new = set((self.state / 'runs').iterdir()) - old if (self.state / 'runs').exists() else set()
                clean = (len(originals) == len(new) == 1 and after == before and
                    all(value == after for value in originals.values()) and free == self.locks and
                    all(not (run / 'output/msr-original.json').exists() for run in new) and not errors)
                save(phase / 'cleanup.json', {'clean': clean, 'before': before, 'after': after,
                    'owned_originals': {str(k): v for k, v in originals.items()}, 'locks': free, 'errors': errors})
                if not clean:
                    raise RuntimeError('owned restoration/lease evidence failed or incomplete')
                self.last_run = next(iter(new))
        if life is None or life['failure'] or life['manager_handoff'] or not life['leader_reaped'] or not life['owned_session_gone']:
            raise RuntimeError('owned command failed or cleanup uncertain')
        save(phase / 'wall.json', {'wall_seconds': time.monotonic() - started})
        self.remaining()
        return life['exit'], (phase / 'stdout').read_text(), (phase / 'stderr').read_text()

    def remaining(self):
        if time.monotonic() >= self.deadline - CLEANUP_SECONDS:
            raise TimeoutError('five-minute campaign cleanup reserve reached')

    def inspect(self, name, run):
        code, stdout, _ = self.invoke(name, [str(self.args.cli), 'inspect', str(run), '--json'])
        if code != 0:
            raise RuntimeError('public inspect failed')
        return json.loads(stdout)

    def run(self):
        a = self.args
        runtime = ['--kernel', str(a.kernel), '--vmm', str(a.engine), '--guest-init', str(a.guest_init),
                   '--busybox', str(a.busybox), '--cpu', str(a.cpu), '--state', str(self.state), '--memory-mb', '512']
        wrong = self.root / 'wrong-kernel-input'
        wrong.write_bytes(b'private wrong kernel; real kernel unchanged\n')
        invalid = runtime.copy()
        invalid[1] = str(wrong)
        # This refusal control always uses the pinned engine without dev permission.
        if getattr(a, 'pinned_engine', None) is not None:
            invalid[invalid.index('--vmm') + 1] = str(a.pinned_engine)
        code, stdout, stderr = self.invoke('wrong-kernel', [str(a.cli), 'run', str(REPO / 'examples/faults'), *invalid])
        if code != 2 or stdout or 'guest kernel is outside the admitted host policy' not in stderr:
            raise RuntimeError('wrong kernel did not receive the required policy refusal; stop')
        if self.state.exists() and any(self.state.rglob('manifest.json')):
            raise RuntimeError('wrong kernel dispatched a run')
        dev_engine = getattr(a, 'allow_dev_engine', False)
        dev_flags = ['--allow-dev-engine'] if dev_engine else []
        runtime += dev_flags
        self.cpus = pair(a.cpu)
        self.locks = lock_free(self.cpus)
        pinned_workloads(self.cpus)
        save(self.root / 'pair.json', {'cpus': self.cpus, 'locks': self.locks})
        doctor_args = [str(a.cli), 'doctor', '--kernel', str(a.kernel), '--vmm', str(a.engine), '--cpu', str(a.cpu), *dev_flags]
        code, stdout, _ = self.invoke('doctor', doctor_args)
        if code != 0 or (dev_engine and 'dev engine: unqualified' not in stdout):
            raise RuntimeError('doctor did not report the requested engine mode')
        if dev_engine and re.search(r'(?<!un)qualified', stdout):
            raise RuntimeError('doctor claimed qualification for a dev engine')
        code, stdout, _ = self.invoke('doctor-json', [*doctor_args, '--json'])
        if code != 0 or json.loads(stdout).get('engine_qualification') != ('dev-unpinned' if dev_engine else None):
            raise RuntimeError('doctor JSON qualification differs from smoke mode')
        code, stdout, _ = self.invoke('fixed', [str(a.cli), 'run', str(REPO / 'examples/faults'), '--seed', '1009', '--full-state', *runtime], True)
        if code != 0:
            raise RuntimeError('fixed baseline failed')
        baseline_run = result_directory(stdout, self.state, self.last_run)
        baseline = self.inspect('fixed-inspect', baseline_run)
        streams = verify_record(baseline, baseline_run, self.state)
        verify_qualification(baseline, dev_engine)
        if dev_engine and 'engine_qualification: dev-unpinned (unqualified dev engine)' not in (self.root / 'fixed/stdout').read_text():
            raise RuntimeError('dev engine label missing from public run output')
        if baseline['identity']['inputs']['kernel'] != digest(a.kernel) or baseline['identity']['inputs']['profile']['engine'] != digest(a.engine):
            raise RuntimeError('executed kernel/engine differ from frozen supplied bytes')
        for name, flags, parameter, value in [('default', [], None, None), ('margin', ['--margin', '32768'], 'SEEDBOX_MARGIN', '32768'), ('undo', ['--undo', '0'], 'SEEDBOX_UNDO', '0')]:
            code, stdout, _ = self.invoke(name, [str(a.cli), 'replay', str(baseline_run), '--cpu', str(a.cpu), '--state', str(self.state), *dev_flags, *flags], True)
            if code != 0:
                raise RuntimeError('retained replay failed')
            run = result_directory(stdout, self.state, self.last_run)
            record = self.inspect(name + '-inspect', run)
            compare(baseline, record, streams, verify_record(record, run, self.state))
            verify_qualification(record, dev_engine)
            if dev_engine and 'replay: MATCH (unqualified dev engine)' not in stdout:
                raise RuntimeError('dev replay is missing its unqualified label')
            verify_performance(baseline, record, parameter, value)
        if pair(a.cpu) != self.cpus:
            raise RuntimeError('sibling topology changed')
        save(self.root / 'final-cleanup.json', {'cpus': self.cpus, 'locks': lock_free(self.cpus, self.locks), 'msrs': snapshot(self.cpus)})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('cli', 'engine', 'kernel', 'guest-init', 'busybox', 'output'):
        parser.add_argument('--' + name, required=True, type=Path)
    parser.add_argument('--cpu', required=True, type=int)
    parser.add_argument('--allow-dev-engine', action='store_true', help='Exercise unpinned artifacts, visibly unqualified.')
    parser.add_argument('--pinned-engine', type=Path, help='Pinned engine for the unflagged wrong-kernel control; required in dev mode.')
    parser.add_argument('--tool-path', required=True, help='Explicit admitted preparation-tool PATH; no build is performed by this wrapper.')
    args = parser.parse_args()
    if sys.platform != 'linux' or os.geteuid() != 0:
        parser.error('requires the qualified Linux AMD host and root')
    for name in ('cli', 'engine', 'kernel', 'guest_init', 'busybox'):
        path = getattr(args, name).resolve(strict=True)
        if not path.is_file():
            parser.error(name + ' must name an existing frozen file')
        setattr(args, name, path)
    if args.allow_dev_engine and args.pinned_engine is None:
        parser.error('--allow-dev-engine requires --pinned-engine for the wrong-kernel refusal control')
    if args.pinned_engine is not None:
        args.pinned_engine = args.pinned_engine.resolve(strict=True)
        if not args.pinned_engine.is_file():
            parser.error('--pinned-engine must name an existing frozen file')
    root = args.output.absolute()
    root.mkdir(mode=0o700)  # Never reuse or write failure receipts into an existing directory.
    (root / 'home').mkdir()
    began = time.monotonic()
    deadline = began + TOTAL_SECONDS
    old = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGALRM)}
    def interrupted(sig, frame):
        raise TimeoutError('campaign deadline or catchable interruption: ' + str(sig))
    try:
        for sig in old:
            signal.signal(sig, interrupted)
        signal.setitimer(signal.ITIMER_REAL, TOTAL_SECONDS - CLEANUP_SECONDS)
        save(root / 'inputs.json', {name: {'path': str(getattr(args, name)), 'sha256': digest(getattr(args, name))}
             for name in ('cli', 'engine', 'kernel', 'guest_init', 'busybox')})
        save(root / 'mode.json', {'allow_dev_engine': args.allow_dev_engine,
            'pinned_engine': None if args.pinned_engine is None else {'path': str(args.pinned_engine), 'sha256': digest(args.pinned_engine)}})
        Campaign(args, root, deadline).run()
        if time.monotonic() >= deadline - CLEANUP_SECONDS:
            raise TimeoutError('final evidence exhausted campaign allocation')
        save(root / 'result.json', {'status': 'COMPLETE_PENDING_INDEPENDENT_REVIEW', 'wall_seconds': time.monotonic() - began})
        print('complete: staged smoke evidence requires independent review')
        return 0
    except BaseException as error:
        save(root / 'failure.json', {'error': repr(error), 'stop_later_operations': True})
        raise
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        for sig, handler in old.items():
            signal.signal(sig, handler)


if __name__ == '__main__':
    sys.exit(main())
