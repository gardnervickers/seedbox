"""Unit checks for guest drivers; hardware acceptance uses seedbox run directly."""
from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
import re
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


race = driver('race')
kafka_broker = driver('kafka_broker')


class ScenarioTests(unittest.TestCase):
    def test_atomic_control_is_checked_inside_race_driver(self):
        for atomic, succeeds in [(0, True), (1, False)]:
            responses = [subprocess.CompletedProcess([], 0, 'RACE mode=racy counter=7 expected=10 lost=3\n'),
                         subprocess.CompletedProcess([], 0, f'RACE mode=atomic counter={10-atomic} expected=10 lost={atomic}\n')]
            with patch.object(race.subprocess, 'run', side_effect=responses), patch('builtins.print'):
                if succeeds: race.main()
                else:
                    with self.assertRaises(RuntimeError): race.main()


    def test_kafka_log_check_requires_each_acknowledged_record_once_in_order(self):
        values = [kafka_broker.value(seq) for seq in range(4)]
        log = [(offset, 'k', value) for offset, value in enumerate(values)]
        kafka_broker.exactly(log, values, 'topic')
        for broken in (log[:-1], log + [(4, 'k', values[0])], [log[1], log[0]] + log[2:],
                       [(offset + 1, key, value) for offset, key, value in log]):
            with self.assertRaises(kafka_broker.Failed):
                kafka_broker.exactly(broken, values, 'topic')

    def test_checkpoint_oracles_mark_their_capture_fault(self):
        # applications.rs captures each checkpoint-* workload at the first fault
        # after the oracle's CAPTURE_NEXT line and a non-fault call; an oracle
        # without that line runs to completion before any capture.
        workloads = sorted((REPO / 'harness/acceptance/workloads').glob('checkpoint-*.yaml'))
        self.assertTrue(workloads)
        for workload in workloads:
            with self.subTest(workload=workload.name):
                oracle = workload.read_text().split('\noracle:\n', 1)[1]
                source = re.search(r'source: "(\.\./driver/[^"]+\.py)"', oracle).group(1)
                lines = [line.strip() for line in (workload.parent / source).read_text().splitlines()]
                marked = lines.index('print("CAPTURE_NEXT", flush=True)')
                self.assertEqual(lines[marked + 1], 'world.now_ns()')
                self.assertTrue(any('CAPTURE_PHASE' in line for line in lines[marked + 2:]))


if __name__ == '__main__':
    unittest.main()
