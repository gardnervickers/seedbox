"""Unit checks for guest drivers; hardware acceptance uses seedbox run directly."""
from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
from types import SimpleNamespace
import subprocess
import sys
import unittest
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / 'python'))


def driver(name):
    spec = spec_from_file_location(name, REPO / f'harness/acceptance/driver/{name}.py')
    module = module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


etcd, race = driver('etcd'), driver('race')


class World:
    def __init__(self, drop=True):
        self.events, self.active, self.drops, self.drop = [], [], 10, drop

    def sleep(self, seconds):
        if self.active and self.drop:
            self.drops += 1

    def stop(self, target): self.events.append(('stop', target))
    def restart(self, target): self.events.append(('restart', target))

    def partition(self, left, right):
        self.events.append(('partition', left, right))
        self.active = ['cut']
        return SimpleNamespace(handle='cut')

    def heal(self, cut):
        self.events.append(('heal', cut.handle))
        self.active = []

    def inspect(self):
        return {'active_handles': self.active,
                'network_counters': {'n1->n2': {'partition_dropped': self.drops}}}


class ScenarioTests(unittest.TestCase):
    def test_fault_driver_rotates_targets_and_heals_cuts(self):
        world = World()
        etcd.inject(world, ['n1', 'n2', 'n3'], 2, SimpleNamespace(poll=lambda: None))
        self.assertEqual([e[0] for e in world.events], ['stop', 'restart', 'partition', 'heal'] * 2)
        self.assertEqual([e[1] for e in world.events if e[0] == 'stop'], ['n1', 'n2'])

    def test_existing_drops_cannot_hide_ineffective_partitions(self):
        with self.assertRaisesRegex(RuntimeError, 'dropped no'):
            etcd.inject(World(drop=False), ['n1', 'n2', 'n3'], 2, SimpleNamespace(poll=lambda: None))

    def test_workload_must_overlap_faults(self):
        with self.assertRaisesRegex(RuntimeError, 'exited before'):
            etcd.inject(World(), ['n1', 'n2', 'n3'], 2, SimpleNamespace(poll=lambda: 0))

    def test_checker_result_and_real_writes_are_required(self):
        self.assertEqual(etcd.verify('RESULT pass acked=2 ok\n', 0, False), 0)
        for text, code in [('RESULT pass acked=0 ok\n', 0), ('RESULT fail acked=9 missing\n', 0),
                           ('RESULT pass acked=2 ok\nRESULT pass acked=2 ok\n', 0)]:
            with self.assertRaises(RuntimeError): etcd.verify(text, code, False)

    def test_negative_control_must_detect_its_planted_acknowledgement(self):
        self.assertEqual(etcd.verify('RESULT fail acked=2 missing k/99/000000@0\n', 1, True), 1)
        for text, code in [('RESULT pass acked=2 ok\n', 0),
                           ('RESULT fail acked=2 unrelated failure\n', 1),
                           ('RESULT fail acked=1 missing k/99/000000@0\n', 1)]:
            with self.assertRaises(RuntimeError): etcd.verify(text, code, True)

    def test_atomic_control_is_checked_inside_race_driver(self):
        for atomic, succeeds in [(0, True), (1, False)]:
            responses = [subprocess.CompletedProcess([], 0, 'RACE mode=racy counter=7 expected=10 lost=3\n'),
                         subprocess.CompletedProcess([], 0, f'RACE mode=atomic counter={10-atomic} expected=10 lost={atomic}\n')]
            with patch.object(race.subprocess, 'run', side_effect=responses), patch('builtins.print'):
                if succeeds: race.main()
                else:
                    with self.assertRaises(RuntimeError): race.main()


if __name__ == '__main__':
    unittest.main()
