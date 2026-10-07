"""Portable pair allocation and flock contention; no real CPU or MSR access."""
import fcntl
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import host_pairs


def topology():
    return {cpu: {cpu % 16, cpu % 16 + 16} for cpu in range(32)}


class PairTests(unittest.TestCase):
    def test_5950x_excludes_zero_and_sixteen_deduplicates_siblings(self):
        pairs = host_pairs.eligible_pairs(topology(), set(range(32)), set(range(32)))
        self.assertEqual(pairs, [(cpu, cpu + 16) for cpu in range(1, 16)])
        self.assertEqual(len(set(cpu for pair in pairs for cpu in pair)), 30)

    def test_offline_disallowed_and_nonreciprocal_pairs_excluded(self):
        tree = topology()
        tree[18] = {18, 3}
        pairs = host_pairs.eligible_pairs(tree, set(range(32)) - {17}, set(range(32)) - {19})
        self.assertNotIn((1, 17), pairs)
        self.assertNotIn((2, 18), pairs)
        self.assertNotIn((3, 19), pairs)
        self.assertEqual(len(pairs), 12)

    def test_topology_read_is_injectable(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'online').write_text('0-3,16-19')
            for cpu in (0, 1, 2, 3, 16, 17, 18, 19):
                path = root / f'cpu{cpu}/topology'
                path.mkdir(parents=True)
                (path / 'thread_siblings_list').write_text(f'{cpu % 16},{cpu % 16 + 16}')
            self.assertEqual(host_pairs.read_pairs(root, {2, 18}), [(2, 18)])

    def test_busy_pair_skipped_reservations_remain_held_and_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for cpu in (1, 17, 2, 18, 3, 19):
                (root / f'seedbox-msr-cpu-{cpu}.lock').touch()
            with (root / 'seedbox-msr-cpu-17.lock').open() as busy:
                fcntl.flock(busy, fcntl.LOCK_EX | fcntl.LOCK_NB)
                with host_pairs.allocate_pairs([(1, 17), (2, 18), (3, 19)], 2, root) as pairs:
                    self.assertEqual(pairs, [(2, 18), (3, 19)])
                    # Canonical locks must be free for the execution-owning child.
                    host_pairs.probe_canonical((2, 18), root)
                    with self.assertRaises(BlockingIOError):
                        with host_pairs.coordinator_lock((2, 18), root):
                            pass
                    with self.assertRaisesRegex(RuntimeError, 'only 0'):
                        with host_pairs.allocate_pairs([(2, 18), (3, 19)], 1, root):
                            pass
            with host_pairs.coordinator_lock((2, 18), root):
                pass
            self.assertTrue((root / 'seedbox-msr-cpu-17.lock').exists())

    def test_insufficient_pairs_releases_partial_reservation_no_dispatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for cpu in (1, 17):
                (root / f'seedbox-msr-cpu-{cpu}.lock').touch()
            with self.assertRaisesRegex(RuntimeError, 'only 1'):
                with host_pairs.allocate_pairs([(0, 16), (1, 17)], 2, root):
                    self.fail('partial allocation dispatched')
            with host_pairs.coordinator_lock((1, 17), root):
                pass

    def test_missing_symlink_and_nonregular_leases_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaises(FileNotFoundError):
                host_pairs.probe_canonical((1, 17), root)
            self.assertFalse(list(root.iterdir()))
            path = root / 'seedbox-msr-cpu-1.lock'
            path.symlink_to(root / 'target')
            with self.assertRaises(OSError):
                host_pairs.probe_canonical((1, 17), root)
            path.unlink()
            path.mkdir()
            with self.assertRaises((OSError, RuntimeError)):
                host_pairs.probe_canonical((1, 17), root)

    def test_invalid_count_and_probe_failure_release_reservations(self):
        for count in (0, 16):
            with self.assertRaises(ValueError):
                with host_pairs.allocate_pairs([], count):
                    pass
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaisesRegex(RuntimeError, 'pinned'):
                with host_pairs.allocate_pairs([(1, 17)], 1, root, probe=lambda p: None,
                                                idle=lambda p: (_ for _ in ()).throw(RuntimeError('pinned'))):
                    pass
            with host_pairs.coordinator_lock((1, 17), root):
                pass


if __name__ == '__main__':
    unittest.main()
