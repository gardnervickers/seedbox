"""Portable checks for the directed-link fixtures and their host checker."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import subprocess
import os
import unittest

REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('links', REPO / 'harness/links.py')
links = importlib.util.module_from_spec(spec)
spec.loader.exec_module(links)


def world(name):
    return json.loads((REPO / f'harness/worlds/{name}.json').read_text())


class LinkTests(unittest.TestCase):
    def test_full_probe_and_world_bytes_preserve_the_train_inputs(self):
        # SHA-256 of the original 14aab0c controls: avoid moving the full lanes
        # when shortening only the undo pair.
        hashes = {
            'harness/worlds/shaped.json': 'ad76fbf58726a9762f48dd7dc118781039102559270fea503f6aace333fcbce7',
            'harness/worlds/plain.json': '65c7098f06164a31240db530b351813fa48b5ab604c02071e0f2c40a18298f8a',
            'harness/fixtures/links/link-probe.c': '936a38e5a5ac25235a1c8db6ca0f6289132c7a8f394dc3878f8d9c7317212194',
            'harness/fixtures/links/link.init': '81351fc6976da41fef355752761b93e1d421d0597b7f5f5ecabb12a0feed4948',
        }
        for path, digest in hashes.items():
            self.assertEqual(hashlib.sha256((REPO / path).read_bytes()).hexdigest(), digest, path)

    def test_short_world_preserves_all_sixteen_faults_and_loss_exposure(self):
        full, short = world('shaped-tol'), world('shaped-tol-short')
        self.assertEqual(len(short['faults']), 16)
        self.assertEqual([f['id'] for f in short['faults']], list(range(1, 17)))
        self.assertEqual([f['at_ns'] for f in short['faults']],
                         sorted(f['at_ns'] for f in short['faults']))
        self.assertEqual(short['faults'][0]['at_ns'], 300_000_000)
        self.assertEqual(short['faults'][-1]['at_ns'], 1_000_000_000)
        for old, new in zip(full['faults'], short['faults']):
            self.assertEqual({k: v for k, v in old.items() if k not in ('at_ns', 'deadline_ns')},
                             {k: v for k, v in new.items() if k not in ('at_ns', 'deadline_ns')})
            self.assertEqual(new['deadline_ns'], new['at_ns'])
        self.assertEqual(short['network'], full['network'])
        self.assertEqual(short['stop_tolerance_ns'], 200_000)
        self.assertEqual(short['duration_ns'], 2_500_000_000)
        self.assertEqual(links.policy_at(short, 'c', 'a', 600_000_000),
                         (1_000_000, 500_000, 500_000))
        # Only uncut sends consume lossmid draws. Estimate at the configured
        # probe gap, not 50% receipts, and exclude the c|a cut's whole interval.
        exposed = sum(links.policy_at(short, 'c', 'a', t) ==
                      (1_000_000, 500_000, 500_000)
                      for t in range(0, 1_500_000_000, 1_000_000))
        self.assertEqual(exposed, 340)
        self.assertGreaterEqual(exposed, 300)
        init = (REPO / 'harness/fixtures/links/link-short.init').read_text()
        self.assertIn('link-probe "$name" 1000000 1500000000', init)
    def test_full_world_plan_preserves_every_direction_and_fifo_transition(self):
        plan = links.window_plan(world('shaped'))
        def windows(parts):
            return {w: value for lo, hi, value in parts for w in range(lo, hi + 1)}
        self.assertEqual(plan, {
            ('b', 'a'): windows([(20, 49, (3_000_000, 0, 1))]),
            ('a', 'b'): windows([(20, 49, (1_000_000, 0, 1))]),
            ('c', 'a'): windows([(20, 24, (1_000_000, 0, 1)), (25, 29, (5_000_000, 0, 1)),
                                (30, 39, (8_000_000, 0, 1)), (40, 40, (None, 0, 1)),
                                (41, 42, (1_000_000, 0, 1)), (43, 44, (None, 0, 0)),
                                (45, 49, (1_000_000, 0, 1))]),
            ('c', 'b'): windows([(20, 24, (1_000_000, 0, 1)), (25, 29, (5_000_000, 0, 1)),
                                (30, 34, (8_000_000, 0, 1)), (35, 35, (None, 0, 1)),
                                (36, 39, (5_000_000, 0, 1)), (40, 40, (None, 0, 1)),
                                (41, 49, (1_000_000, 0, 1))]),
            ('a', 'c'): windows([(20, 39, (1_000_000, 0, 1)),
                                (40, 42, (1_000_000, 500_000, .5)), (43, 44, (None, 0, 0)),
                                (45, 46, (1_000_000, 500_000, .5)), (47, 47, (None, 0, 1)),
                                (48, 49, (1_000_000, 0, 1))]),
            ('b', 'c'): windows([(20, 39, (1_000_000, 0, 1)), (40, 46, (1_000_000, 0, 0)),
                                (47, 49, (1_000_000, 0, 1))]),
        })

    def test_window_45_checks_loss_floor_mean_and_loose_receipt_sanity(self):
        expected = links.window_plan(world('shaped'))[('a', 'c')][45]
        # Reported train/3 sample: already the correct half-loss distribution.
        self.assertTrue(links.window_matches(expected, (207, 1.004, 1.64, 1.25), 400))
        for observed in [(400, 1.004, 1.64, 1.25), (0, 0, 0, 0),
                         (100, 1.004, 1.64, 1.25), (207, .9, 1.64, 1.25),
                         (207, 1.2, 1.64, 1.25), (207, 1.004, 1.701, 1.25),
                         (207, 1.004, 2.5, 2.0)]:
            self.assertFalse(links.window_matches(expected, observed, 400), observed)
        self.assertFalse(links.window_matches(expected, (207, 1.004, 1.64, 1.25), 0))
        self.assertFalse(links.window_matches((None, 0, 0), (5, 1, 1, 1), 400))
        self.assertFalse(links.window_matches((1_000_000, 0, 1), (390, 1, 1, 1), 400))


    def test_exact_routed_bounds_include_the_ceiling_and_only_one_ns_of_fifo(self):
        full = world('shaped')
        send = 4_500_000_000
        ceiling = send + 1_500_000
        def record(t, at):
            return dict(src='c', dst='a', send_ns=t, arrival_ns=at)
        at_ceiling = record(send, ceiling)
        fifo = record(send, ceiling + 1)
        self.assertEqual(links.arrival_bounds(full, [at_ceiling, fifo]), {('c', 'a', 45): 2})
        for records in ([record(send, ceiling + 1)],
                        [at_ceiling, record(send, ceiling + 2)],
                        [at_ceiling, record(send, ceiling)],
                        [record(send, send + 999_999)],
                        [record(4_400_000_000, 4_401_000_000)],
                        [fifo, at_ceiling], []):
            with self.assertRaises(ValueError):
                links.arrival_bounds(full, records)

    def test_routed_bound_handles_policy_decrease_and_independent_pairs(self):
        full = world('shaped')
        def record(src, dst, send, at):
            return dict(src=src, dst=dst, send_ns=send, arrival_ns=at)
        records = [record('a', 'c', 3_999_999_999, 4_007_999_999),
                   record('a', 'c', 4_000_000_000, 4_008_000_000),
                   record('b', 'a', 3_999_000_000, 4_000_000_000)]
        self.assertEqual(sum(links.arrival_bounds(full, records).values()), 3)
        records[1]['arrival_ns'] += 1
        with self.assertRaises(ValueError):
            links.arrival_bounds(full, records)

    def shell(self, code, **env):
        return subprocess.run(['bash', '-euo', 'pipefail', '-c',
                               'source harness/run-links.sh\n' + code],
                              cwd=REPO, env={**os.environ, **env},
                              text=True, capture_output=True, timeout=10)

    def test_lane_selection_defaults_subset_and_errors(self):
        default = self.shell('unset RUNS; select_lanes; echo "${lanes[*]}"',
                             CPUS='1 2 3 4 5 6')
        self.assertEqual(default.returncode, 0, default.stderr)
        self.assertEqual(default.stdout.strip(), 'L1 L2 P1 M1 U1 U2')
        subset = self.shell('select_lanes; echo "${lanes[*]}:${cores[*]}"',
                            RUNS='L1 U1', CPUS='3 7')
        self.assertEqual(subset.returncode, 0, subset.stderr)
        self.assertEqual(subset.stdout.strip(), 'L1 U1:3 7')
        for runs, cpus, error in [('L1 NO', '1 2', 'unknown lane'),
                                  ('L1 L1', '1 2', 'duplicate lane'),
                                  ('L1 U1', '1 1', 'duplicate CPU'),
                                  ('L1 U1', '1', 'one distinct physical core'),
                                  ('L1', '-1', 'invalid CPU'),
                                  ('', '', 'at least one lane')]:
            result = self.shell('select_lanes', RUNS=runs, CPUS=cpus)
            self.assertEqual(result.returncode, 2, result)
            self.assertIn(error, result.stderr)

    def test_invocations_keep_private_results_and_preserve_existing_results(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'results').mkdir()
            sentinel = root / 'results/previous'
            sentinel.write_text('retained')
            result = self.shell('''allocate_directories "$TEST_ROOT/results" "$TEST_ROOT"
echo "$RESULTS:$S"
allocate_directories "$TEST_ROOT/results" "$TEST_ROOT"
echo "$RESULTS:$S"
''', TEST_ROOT=temp)
            self.assertEqual(result.returncode, 0, result.stderr)
            directories = [path for line in result.stdout.splitlines() for path in line.split(':')]
            self.assertEqual(len(set(directories)), 4)
            self.assertTrue(all(Path(path).is_dir() for path in directories))
            self.assertEqual(sentinel.read_text(), 'retained')

    def test_parallel_lanes_overlap_and_join_after_a_failure(self):
        with tempfile.TemporaryDirectory() as temp:
            result = self.shell('''select_lanes
S=fixture
RESULTS=$TEST_RESULTS
run() {
  echo "$*" > "$RESULTS/$2.args"
  touch "$RESULTS/$2.started"
  # Every lane must have started before any can finish. Serial dispatch fails.
  for ((n=0; n<100; n++)); do
    count=$(find "$RESULTS" -name '*.started' | wc -l)
    if [[ $count -eq 6 ]]; then break; fi
    sleep .01
  done
  [[ $count -eq 6 ]] || return 9
  touch "$RESULTS/$2.finished"
  [[ $2 != L1 ]]
}
rc=0
run_lanes || rc=$?
echo "joined:$rc:$(find "$RESULTS" -name '*.finished' | wc -l)"
''', RUNS='L1 L2 P1 M1 U1 U2', CPUS='1 2 3 4 5 6', TEST_RESULTS=temp)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip().replace(' ', ''), 'joined:1:6')
            expected = {'L1': '1 L1 shaped fixture/pr 1800',
                        'L2': '2 L2 shaped fixture/pr 1800',
                        'P1': '3 P1 plain fixture/pr 1800',
                        'M1': '4 M1 plain fixture/main 1800',
                        'U1': '5 U1 shaped-tol-short fixture/pr 600',
                        'U2': '6 U2 shaped-tol-short fixture/pr 600 SEEDBOX_UNDO=0'}
            for lane, args in expected.items():
                self.assertEqual((Path(temp) / f'{lane}.args').read_text().strip(), args)

    def test_host_checker_returns_failure_for_broken_lane_evidence(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            full, short = world('shaped'), world('shaped-tol-short')
            (root / 'shaped.json').write_text(json.dumps(full))
            (root / 'shaped-tol-short.json').write_text(json.dumps(short))
            for run in ('L1', 'L2', 'P1', 'M1', 'U1', 'U2'):
                shaped = run[0] in 'LU'
                report = {
                    'outcome': 'end_machine_exited',
                    'world_witness': 'short' if run[0] == 'U' else 'shaped' if shaped else 'plain',
                    'faults': [dict(id=f['id'], status='applied', applied_at_ns=f['at_ns'])
                               for f in (short if run[0] == 'U' else full)['faults']] if shaped else [],
                    'network_counters': {'c->a': {'loss_dropped': 10, 'partition_dropped': 10},
                                         'c->b': {'loss_dropped': 10},
                                         'a->c': {'partition_dropped': 10}} if shaped else {},
                }
                (root / f'{run}.json').write_text(json.dumps(report))
                (root / run).mkdir()
                console = '\n'.join(f'LINK-SENT {src} w={w} n=400'
                                    for src in 'abc' for w in range(20, 50)) + '\n'
                for (dst, src), windows in links.window_plan(full).items():
                    console += f'LINK-SRC {dst}<-{src} reorders=0\n'
                    for w, (lat, jitter, frac) in windows.items():
                        if not frac:
                            continue
                        n = int(400 * frac)
                        lo = lat or 1_000_000
                        mean = lo + jitter // 2
                        hi = lo + jitter
                        if (dst, src, w) == ('a', 'c', 45):
                            n, lo, hi, mean = 207, 1_004_000, 1_640_000, 1_250_000
                        console += (f'LINK-WIN {dst}<-{src} w={w} n={n} '
                                    f'min={lo} max={hi} mean={mean}\n')
                (root / run / 'all.console').write_text(console)
                (root / run / 'network.jsonl').write_text(json.dumps(dict(
                    src='c', dst='a', send_ns=4_500_000_000, arrival_ns=4_501_500_000)) + '\n')
            def analyze():
                with contextlib.redirect_stdout(io.StringIO()):
                    return links.analyze(root)
            self.assertEqual(analyze(), 0)
            with contextlib.redirect_stdout(io.StringIO()) as output:
                self.assertEqual(links.analyze(root, ['L1']), 0)
            self.assertIn('SKIP shaped replay', output.getvalue())
            trace = root / 'L1/network.jsonl'
            before = trace.read_text()
            trace.unlink()
            self.assertEqual(analyze(), 1)
            trace.write_text(before.replace('4501500000', '4501500001'))
            self.assertEqual(analyze(), 1)
            trace.write_text(before)
            with self.assertRaises(ValueError):
                links.analyze(root, ['unknown'])
            for name, change in [
                    ('P1', lambda r: r['network_counters'].update({'a->b': {'loss_dropped': 1}})),
                    ('U2', lambda r: r['faults'][0].update(status='failed')),
                    ('L2', lambda r: r['faults'][0].update(applied_at_ns=1)),
                    ('U1', lambda r: r.update(world_witness='different'))]:
                path = root / f'{name}.json'
                before = path.read_text()
                report = json.loads(before)
                change(report)
                path.write_text(json.dumps(report))
                self.assertEqual(analyze(), 1, name)
                path.write_text(before)
            path = root / 'U2/all.console'
            path.write_text(path.read_text().replace('reorders=0', 'reorders=1', 1))
            self.assertEqual(analyze(), 1)

if __name__ == '__main__':
    unittest.main()
