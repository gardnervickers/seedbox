"""Portable checks for the directed-link fixtures and their host checker."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
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
        runner = (REPO / 'harness/run-links.sh').read_text()
        for line in ('run L1 shaped "$S/pr" 1800', 'run L2 shaped "$S/pr" 1800',
                     'run P1 plain "$S/pr" 1800', 'run M1 plain "$S/main" 1800',
                     'run U1 shaped-tol-short "$S/pr" 600',
                     'run U2 shaped-tol-short "$S/pr" 600 SEEDBOX_UNDO=0'):
            self.assertIn(line, runner)

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

    def test_window_45_checks_loss_floor_and_mean_not_an_unbounded_receipt_tail(self):
        expected = links.window_plan(world('shaped'))[('a', 'c')][45]
        # Reported train/3 sample: already the correct half-loss distribution.
        self.assertTrue(links.window_matches(expected, (207, 1.004, 1.64, 1.25), 400))
        for observed in [(400, 1.004, 1.64, 1.25), (0, 0, 0, 0),
                         (100, 1.004, 1.64, 1.25), (207, .9, 1.64, 1.25),
                         (207, 1.2, 1.64, 1.25), (207, 1.004, 2.5, 2.0)]:
            self.assertFalse(links.window_matches(expected, observed, 400), observed)
        self.assertFalse(links.window_matches(expected, (207, 1.004, 1.64, 1.25), 0))
        self.assertFalse(links.window_matches((None, 0, 0), (5, 1, 1, 1), 400))
        self.assertFalse(links.window_matches((1_000_000, 0, 1), (390, 1, 1, 1), 400))


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
            def analyze():
                with contextlib.redirect_stdout(io.StringIO()):
                    return links.analyze(root)
            self.assertEqual(analyze(), 0)
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
