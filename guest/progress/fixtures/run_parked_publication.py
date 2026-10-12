"""Run the bounded Linux/ARM DR ownership discriminator (no build or VM launch)."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

from check_parked_publication import check, check_application_return


def execute(command, log, target=None, diagnostic=False):
    start = time.monotonic()
    sampled = False
    evidence = {}
    timed_out = False
    with log.open('w') as stream:
        process = subprocess.Popen(command, stdout=stream, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        try:
            while process.poll() is None and time.monotonic() - start < 20:
                if not sampled:
                    text = log.read_text()
                    if ('BEGIN ' in text or ('repro park: target=' in text and
                                             time.monotonic() - start > 0.5)):
                        proc = Path(f'/proc/{process.pid}')
                        try:
                            (log.with_suffix('.maps')).write_text((proc / 'maps').read_text())
                            evidence['maps'] = str(log.with_suffix('.maps'))
                            evidence['tasks'] = {}
                            for task in (proc / 'task').iterdir():
                                try:
                                    evidence['tasks'][task.name] = (task / 'syscall').read_text().strip()
                                except (FileNotFoundError, ProcessLookupError):
                                    pass
                            if target is not None and target.exists():
                                addresses = [int(x, 16) for x in target.read_text().split()[:3]]
                                with (proc / 'mem').open('rb', buffering=0) as memory:
                                    for name, address in zip(('first_word', 'req', 'ack'), addresses):
                                        memory.seek(address)
                                        evidence[name] = int.from_bytes(memory.read(4), 'little')
                            sampled = True
                        except (FileNotFoundError, ProcessLookupError):
                            pass  # A completed positive still needs full output and exit 0.
                time.sleep(0.005)
            if process.poll() is None:
                timed_out = True
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
            code = process.wait(timeout=2)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=2)
    return {'command': list(map(str, command)), 'returncode': code, 'timeout': timed_out,
            'elapsed_seconds': time.monotonic() - start, 'log': str(log),
            'diagnostic': diagnostic, 'observation': evidence}


def gencode_read_proof(directory, pc):
    for path in directory.rglob('gencode-shared*'):
        text = path.read_text()
        match = re.search(r'do_syscall:\n(.*?)(?=\n\S+:)', text, re.S)
        if match:
            svc = re.search(r'(0x[0-9a-f]+)\s+d4000001\s+svc', match[1])
            if svc and int(svc[1], 16) + 4 == pc:
                return {'path': str(path), 'svc': svc[1], 'kernel_pc': hex(pc)}
    raise ValueError('observed kernel read is not the emitted stable do_syscall continuation')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--drrun', required=True, type=Path)
    parser.add_argument('--fixtures', required=True, type=Path)
    parser.add_argument('--progress-client', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--diagnostic', action='store_true')
    parser.add_argument('--negative-controls', action='store_true')
    parser.add_argument('--contract-checks', action='store_true')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    results = []
    recipe = ['--instructions-per-progress-call', '50000', '--progress-points-per-block', '1']
    for mode in ('off', 'on'):
        for case in ('restartable', 'progress', 'application-return'):
            name = f'{case}-{mode}'
            directory = args.output / name
            directory.mkdir()
            core = [str(args.drrun), '-debug', '-opt_speed', '-no_ldstex2cas',
                    '-no_hw_cache_consistency']
            if mode == 'on': core += ['-seedbox_icache_images']
            if args.diagnostic:
                core += ['-gendump', '-loglevel', '2', '-logmask', '0x4518',
                         '-logdir', str(directory)]
            with tempfile.TemporaryDirectory(prefix='seedbox-parked-') as temp:
                target = Path(temp) / 'target'  # Absent until the app's exclusive create.
                if case == 'restartable':
                    command = core + ['-c', str(args.fixtures / 'libparked_publication_client.so'),
                                      str(target), '--', str(args.fixtures / 'parked-publication-context'),
                                      str(target)]
                elif case == 'progress':
                    command = core + ['-c', str(args.fixtures / 'libparked_progress_client.so'),
                                      *recipe, str(target), '--',
                                      str(args.fixtures / 'parked-publication-context'), str(target), 'progress']
                else:
                    command = core + ['-c', str(args.progress_client), *recipe, '--',
                                      str(args.fixtures / 'parked-application-return')]
                result = execute(command, args.output / f'{name}.log',
                                 target if case != 'application-return' else None, args.diagnostic)
                validator = check_application_return if case == 'application-return' else check
                result.update(case=case, mode=mode,
                              metrics=validator(Path(result['log']).read_text(), result['returncode']))
                if mode == 'on' and not result['metrics']['image_records']:
                    raise ValueError('image mode omitted final reclamation counters')
                if args.diagnostic and case == 'application-return':
                    result['gencode'] = gencode_read_proof(directory, result['metrics']['kernel_read_pc'])
                if args.diagnostic:
                    logs = '\n'.join(p.read_text() for p in directory.rglob('log.*.html'))
                    result['transitions'] = {key: logs.count(key) for key in
                        ('Synch failed!', 'waiting at safe spot', 'predicate-wait owned',
                         'predicate-wait complete', 'progress owned', 'progress added',
                         'progress paid', 'progress refunded', 'progress record released')}
                    counts = result['transitions']
                    if not (counts['progress owned'] == counts['progress refunded'] ==
                            counts['progress record released']):
                        raise ValueError('unreleased progress obligations')
                results.append(result)
                (args.output / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
                print(f'{name}: completed=20 exit=0', flush=True)
        if args.negative_controls:
            # The inherited negative is byte-identical and has a fixed diagnostic path.
            # Run only in an exclusively owned test environment; never remove a pre-existing file.
            target = Path('/tmp/repro-park-target')
            if target.exists(): raise ValueError('stale /tmp/repro-park-target: refusing negative control')
            core = [str(args.drrun), '-debug', '-opt_speed', '-no_ldstex2cas', '-no_hw_cache_consistency']
            if mode == 'on': core += ['-seedbox_icache_images']
            command = core + ['-c', str(args.fixtures / 'libparked_raw_client.so'), 'redirect', '--',
                              str(args.fixtures / 'parked-raw-context'), 'call', '1', '0', '1']
            try:
                result = execute(command, args.output / f'raw-{mode}.log', target)
                result.update(case='raw-negative', mode=mode)
                observation = result['observation']
                if (not result['timeout'] or result['returncode'] == 0 or
                    observation.get('first_word') != 0xc85f7d49 or
                    observation.get('req') != 1 or observation.get('ack') != 1):
                    raise ValueError('negative did not reproduce the acknowledged publication cycle')
                results.append(result)
                (args.output / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
                print(f'raw-{mode}: expected acknowledged timeout', flush=True)
            finally:
                target.unlink(missing_ok=True)
        if args.contract_checks:
            diagnostics = {1: 'marked progress payment failed', 2: 'recursive progress obligation',
                           3: 'Received SIGSEGV at client library pc',
                           5: 'invalid progress quantum or charge',
                           6: "progress budget must be this thread's allocated raw TLS",
                           7: 'invalid progress quantum or charge', 8: 'progress exit must be due'}
            for test in range(1, 9):
                with tempfile.TemporaryDirectory(prefix='seedbox-progress-contract-') as temp:
                    target = Path(temp) / 'target'
                    core = [str(args.drrun), '-debug', '-opt_speed', '-no_ldstex2cas',
                            '-no_hw_cache_consistency']
                    if mode == 'on': core += ['-seedbox_icache_images']
                    command = core + ['-c', str(args.fixtures / 'libparked_progress_client.so'),
                                      *recipe, str(target), str(test), '--',
                                      str(args.fixtures / 'parked-publication-context'), str(target),
                                      'zero' if test == 4 else 'progress']
                    result = execute(command, args.output / f'contract-{test}-{mode}.log')
                    text = Path(result['log']).read_text()
                    if result['timeout']:
                        raise ValueError('contract check timed out: only raw negative may time out')
                    if test == 4:
                        zero = re.findall(r'^ZERO iteration=(\d+) charged=5 budget=0 payments=0 '
                                          r'all_gpr=31 all_q=32 bad=0$', text, re.M)
                        if result['returncode'] != 0 or zero != list(map(str, range(1, 21))) or \
                           'END case=zero completed=20 faults=0 signals=0 bad=0' not in text:
                            raise ValueError('zero-not-due fast path failed')
                    elif result['returncode'] != 255 or diagnostics[test] not in text or 'END case=' in text:
                        raise ValueError(f'contract rejection {test} was not fail-closed')
                    result.update(case=f'contract-{test}', mode=mode)
                    results.append(result)
                    (args.output / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
                    print(f'contract-{test}-{mode}: expected exit={result["returncode"]}', flush=True)
    print('bounded discriminator complete; not a PV/replay/performance qualification')


if __name__ == '__main__':
    main()
