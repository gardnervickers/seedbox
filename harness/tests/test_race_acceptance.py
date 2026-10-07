"""Reject incomplete or confounded evidence in the focused race qualification."""
from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
import copy
import unittest

spec = spec_from_file_location('race_cases', Path(__file__).resolve().parents[2] / 'crates/seedbox/tests/race-cases.py')
race = module_from_spec(spec)
spec.loader.exec_module(race)


def fixture():
    record = {'identity': {'seed': 1, 'configuration': {'schedule': 1},
                          'inputs': {'guests': {'n1': 'g'}, 'placement': {'n1': 'n1'},
                                     'images': {'n1': 'i'}, 'profile': {'engine': 'e'}}},
              'outcome': 'PASS', 'report': {'reason': 'COMPLETED',
              'workload_exit': {'service': 'n1', 'code': 0}, 'witness': 'a'*64,
              'full_state': {'n1': 'c'*64}}}
    raw = {'seed': 1, 'outcome': 'end_machine_exited', 'world_witness': 'a'*64,
           'machines': [{'name': 'n1', 'state': 'Exited(0)', 'witness': 'b'*64, 'state_digest': 'c'*64}]}
    console = 'RACE mode=racy counter=7 expected=10 lost=3\nRACE mode=atomic counter=10 expected=10 lost=0\n'
    return record, raw, console


class RaceAcceptanceTests(unittest.TestCase):
    def test_complete_evidence_and_diverse_positive_loss(self):
        result = race.evidence(*fixture(), 1)
        self.assertEqual(result['loss'], 3)
        self.assertEqual(race.diverse([{'loss': 0}, result, {'loss': 2}, result]), 1)
        for results in ([{'loss': 0}]*4, [result]*4):
            with self.assertRaises(race.Unresolved): race.diverse(results)

    def test_missing_duplicate_and_atomic_results_are_rejected(self):
        record, raw, console = fixture()
        for broken in ('', console + console, console.replace('counter=10 expected=10 lost=0', 'counter=9 expected=10 lost=1'),
                       console.replace('counter=7', 'counter=6')):
            with self.assertRaises(ValueError): race.evidence(record, raw, broken, 1)

    def test_outcomes_wrong_seed_and_incomplete_evidence_are_rejected(self):
        for mutation in ('ERROR', 'TIMEOUT', 'INCOMPLETE', 'FAIL'):
            record, raw, console = fixture(); record['outcome'] = mutation
            with self.assertRaises(race.Unresolved): race.evidence(record, raw, console, 1)
        for field in ('world_witness', 'machine', 'duplicate', 'state_digest', 'witness', 'seed', 'full_state'):
            record, raw, console = fixture()
            if field == 'machine': raw['machines'] = []
            elif field == 'duplicate': raw['machines'] *= 2
            elif field in ('state_digest', 'witness'): raw['machines'][0].pop(field)
            elif field == 'full_state': record['report']['full_state'] = {}
            elif field == 'seed': raw['seed'] = 2
            else: raw.pop(field)
            with self.assertRaises(ValueError): race.evidence(record, raw, console, 1)

    def test_seed_is_the_only_admitted_identity_change(self):
        baseline = fixture()[0]
        changed = copy.deepcopy(baseline); changed['identity']['seed'] = 2
        race.same_inputs(baseline, changed)
        for field in ('guests', 'images', 'profile'):
            changed = copy.deepcopy(baseline); changed['identity']['inputs'][field] = {}
            with self.assertRaises(race.Unresolved): race.same_inputs(baseline, changed)
        changed = copy.deepcopy(baseline); changed['identity']['configuration']['schedule'] = 2
        with self.assertRaises(race.Unresolved): race.same_inputs(baseline, changed)

    def test_preflight_rejects_every_prepared_input_change(self):
        baseline = fixture()[0]['identity']['inputs']
        baseline.update(kernel='k', auxiliary={'context': 'c'}, provenance={'builder': 'v'})
        race.same_prepared(baseline, copy.deepcopy(baseline))
        for field in baseline:
            changed = copy.deepcopy(baseline); changed[field] = {}
            with self.assertRaises(race.Unresolved): race.same_prepared(baseline, changed)
        changed = copy.deepcopy(baseline); changed['unexpected'] = True
        with self.assertRaises(race.Unresolved): race.same_prepared(baseline, changed)

    def test_replay_requires_match_and_each_comparison(self):
        record = fixture()[0]
        record['replay'] = {'status': 'MATCH', **{k: True for k in
            ('outcome_matches', 'witness_matches', 'full_state_matches', 'fault_history_matches')}}
        result = race.evidence(*fixture(), 1)
        race.reproduced(result, result, record)
        for key in record['replay']:
            changed = copy.deepcopy(record); changed['replay'].pop(key)
            with self.assertRaises(ValueError): race.reproduced(result, result, changed)
        for key in result:
            changed = copy.deepcopy(result); changed[key] = 0
            with self.assertRaises(ValueError): race.reproduced(result, changed, record)


if __name__ == '__main__': unittest.main()
