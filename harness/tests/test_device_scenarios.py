"""The device scenario oracles accept correct evidence and reject each violation."""
import base64
from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
import unittest

REPO = Path(__file__).resolve().parents[2]


def driver(name):
    spec = spec_from_file_location(name, REPO / f'harness/acceptance/driver/{name}.py')
    module = module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


durability = driver('durability')
waits = driver('blocked_waits')
tcp = driver('tcp_checkpoint')
groups = driver('process_groups')
BLOCK = 4096


def complete(gen):
    """What a store that finished every phase of `gen` leaves, and its acknowledgements."""
    d = durability
    files = {d.FILES[p]: d.stream(gen, p, d.COUNT) for p in ('fsync', 'fdatasync', 'odsync', 'plain', 'sfr')}
    files['index.map'] = d.stream(gen, 'msync', d.SLOTS)
    files['prealloc.dat'] = d.stream(gen, 'fallocate', d.SLOTS)
    files['manifest'] = d.manifest(gen, d.VERSIONS)
    files['trunc.log'] = d.stream(gen, 'truncate', d.COUNT)
    files['over.dat'] = b''.join(d.block(gen, 0, b, BLOCK) for b in range(d.BLOCKS))
    numbered = set(range(d.COUNT))
    acks = {'generation': {'dir'}, 'fsync': set(numbered), 'fdatasync': set(numbered),
            'odsync': set(numbered), 'msync': set(range(d.SLOTS)) | {'sized'},
            'fallocate': set(range(d.SLOTS)) | {'sized'}, 'rename': set(range(1, d.VERSIONS + 1)),
            'truncate': {'full', 'cut'}, 'overwrite': {'v0'}}
    return files, acks


def recovered(files_by_gen):
    return {'block': BLOCK, 'files': {f'g{gen:03}': {name: base64.b64encode(data).decode()
                                                    for name, data in files.items()}
                                      for gen, files in files_by_gen.items()}}


class DurabilityOracleTests(unittest.TestCase):
    def check(self, mutate, policy='drop', gen=3):
        files, acks = complete(gen)
        mutate(files, acks)
        return durability.verify(recovered({gen: files}), {gen: acks}, policy)

    def test_complete_generation_passes(self):
        errors, seen = self.check(lambda files, acks: None)
        self.assertEqual(errors, [])
        self.assertEqual(seen[3]['fsync'], durability.COUNT)
        self.assertEqual(seen[3]['overwrite'], {'mixed_blocks': 0, 'new_blocks': 0})

    def test_acknowledged_record_must_survive_untorn(self):
        def lose(files, acks):
            files['fsync.log'] = files['fsync.log'][:-durability.RECORD]
        def tear(files, acks):
            log = bytearray(files['odsync.log'])
            log[70] ^= 1
            files['odsync.log'] = bytes(log)
        for mutate in (lose, tear):
            errors, _ = self.check(mutate)
            self.assertTrue(errors, mutate.__name__)

    def test_unacknowledged_bytes_may_be_lost_or_zero_but_not_garbage(self):
        def partial(files, acks):
            acks['fdatasync'] = {0, 1}
            files['fdatasync.log'] = files['fdatasync.log'][:2 * durability.RECORD + 10] + b'\0' * 20
        self.assertEqual(self.check(partial)[0], [])

        def garbage(files, acks):
            acks['fdatasync'] = {0, 1}
            files['fdatasync.log'] = files['fdatasync.log'][:3 * durability.RECORD] + b'x'
        self.assertTrue(self.check(garbage)[0])

    def test_rename_is_one_complete_version(self):
        def older(files, acks):
            files['manifest'] = durability.manifest(3, 1)
        def partial(files, acks):
            files['manifest'] = files['manifest'][:100]
        def unacknowledged(files, acks):
            acks['rename'] = set()
            files['manifest'] = durability.manifest(3, 2)
        self.assertTrue(self.check(older)[0])
        self.assertTrue(self.check(partial)[0])
        self.assertEqual(self.check(unacknowledged)[0], [])

    def test_acknowledged_truncation_is_exact(self):
        def kept(files, acks):
            files['trunc.log'] = durability.stream(3, 'truncate', 2 * durability.COUNT)
        self.assertTrue(self.check(kept)[0])
        acks_only_full = lambda files, acks: (acks['truncate'].discard('cut'), kept(files, acks))
        self.assertEqual(self.check(acks_only_full)[0], [])

    def test_mixed_block_fails_only_when_unflushed_sectors_are_dropped(self):
        def mix(files, acks):
            data = bytearray(files['over.dat'])
            new = durability.block(3, 1, 0, BLOCK)
            data[:durability.SECTOR] = new[:durability.SECTOR]
            files['over.dat'] = bytes(data)
        self.assertTrue(self.check(mix, 'drop')[0])
        errors, seen = self.check(mix, 'torn')
        self.assertEqual(errors, [])
        self.assertEqual(seen[3]['overwrite']['mixed_blocks'], 1)

    def test_whole_new_block_is_allowed_under_both_policies(self):
        def newer(files, acks):
            files['over.dat'] = durability.block(3, 1, 0, BLOCK) + files['over.dat'][BLOCK:]
        for policy in ('drop', 'torn'):
            errors, seen = self.check(newer, policy)
            self.assertEqual(errors, [])
            self.assertEqual(seen[3]['overwrite']['new_blocks'], 1)

    def test_acknowledged_directory_and_unknown_directories(self):
        _, acks = complete(1)
        errors, _ = durability.verify(recovered({}), {1: acks}, 'drop')
        self.assertTrue(errors)
        errors, _ = durability.verify(recovered({7: {}}), {}, 'drop')
        self.assertEqual(errors, ['g007: directory from no generation'])

    def test_crash_plan_covers_every_boundary_and_phase(self):
        import random
        plan = durability.crash_plan(random.Random(1))
        self.assertEqual({k for kind, k, _ in plan if kind == 'boundary'}, set(range(len(durability.PHASES) + 1)))
        self.assertEqual({k for kind, k, _ in plan if kind == 'inside'}, set(range(len(durability.PHASES))))


class WaitOracleTests(unittest.TestCase):
    def ledger(self):
        deadline = 1000
        done = {kind: {'woke_ns': deadline + 10} for kind in waits.KINDS}
        for kind, token in waits.tokens().items():
            done[kind]['token'] = token
        for kind in ('pipe', 'timerfd', 'accept'):
            done[kind]['extra'] = False
        done['epoll'].update(events=1, extra=0)
        done['timerfd']['expirations'] = 1
        done['futex-timeout']['notified'] = False
        return {'done': done, 'deadline_ns': deadline}

    def test_exactly_once_completions_pass(self):
        self.assertEqual(waits.verify(self.ledger(), waits.tokens()), [])

    def test_each_violation_is_reported(self):
        cases = [
            lambda d: d['done'].pop('futex'),
            lambda d: d['done']['pipe'].update(token=999),
            lambda d: d['done']['accept'].update(extra=True),
            lambda d: d['done']['epoll'].update(extra=1),
            lambda d: d['done']['timerfd'].update(expirations=2),
            lambda d: d['done']['futex-timeout'].update(notified=True),
            lambda d: d['done']['nanosleep'].update(woke_ns=999),
        ]
        for mutate in cases:
            ledger = self.ledger()
            mutate(ledger)
            self.assertTrue(waits.verify(ledger, waits.tokens()))


class TcpOracleTests(unittest.TestCase):
    def steps(self):
        import hashlib
        sent = tcp.WARMUP + tcp.FLOOD
        digest = hashlib.sha256(b''.join(tcp.payload(0, s, tcp.DATA_BYTES) for s in range(sent))).hexdigest()
        reply = hashlib.sha256(b''.join(tcp.payload(1, s, tcp.REPLY_BYTES)
                                        for s in range(tcp.REPLY_FRAMES))).hexdigest()
        return {'held': {'data_frames': tcp.WARMUP, 'held_at': 5},
                'flood': {'sent_before_block': 200},
                'continue': {'sent': sent, 'reply_frames': tcp.REPLY_FRAMES, 'reply_digest': reply,
                             'echoes': tcp.TICKS},
                'close': {'summary': {'frames': sent, 'digest': digest}, 'expected_digest': digest,
                          'eof_after_summary': True},
                'reconnect': {'verified': 10},
                'server_end': {'ticks_echoed': tcp.TICKS, 'data_digest': digest}}

    def test_complete_streams_pass(self):
        tcp.verify(self.steps())

    def test_each_violation_is_reported(self):
        cases = [
            lambda s: s['flood'].update(sent_before_block=tcp.WARMUP + tcp.FLOOD),
            lambda s: s['continue'].update(reply_digest='0'),
            lambda s: s['server_end'].update(ticks_echoed=tcp.TICKS + 1),
            lambda s: s['close']['summary'].update(frames=1),
            lambda s: s['close'].update(eof_after_summary=False),
            lambda s: s['server_end'].update(data_digest='0'),
        ]
        for mutate in cases:
            steps = self.steps()
            mutate(steps)
            with self.assertRaises(RuntimeError):
                tcp.verify(steps)

    def test_payloads_depend_on_direction_and_sequence(self):
        self.assertNotEqual(tcp.payload(0, 1, 64), tcp.payload(0, 2, 64))
        self.assertNotEqual(tcp.payload(0, 1, 64), tcp.payload(1, 1, 64))
        self.assertEqual(len(tcp.payload(1, 255, tcp.REPLY_BYTES)), tcp.REPLY_BYTES)


class ProcessGroupOracleTests(unittest.TestCase):
    def statuses(self):
        def status(pid, ppid, pgid, sid):
            return {'pid': pid, 'ppid': ppid, 'pgid': pgid, 'sid': sid}
        return {'leader': status(10, 1, 10, 1), 'member': status(11, 10, 10, 1),
                'orphan': status(13, 1, 10, 1), 'session': status(12, 10, 12, 12)}

    def test_linux_topology_passes(self):
        groups.check_topology(self.statuses())

    def test_topology_violations_are_reported(self):
        cases = [
            lambda s: s['orphan'].update(ppid=10),
            lambda s: s['member'].update(pgid=11),
            lambda s: s['session'].update(sid=1),
            lambda s: s['leader'].update(pgid=1),
        ]
        for mutate in cases:
            statuses = self.statuses()
            mutate(statuses)
            with self.assertRaises(RuntimeError):
                groups.check_topology(statuses)


if __name__ == '__main__':
    unittest.main()
