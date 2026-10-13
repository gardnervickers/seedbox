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
rebuild = driver('disk_rebuild')
journal = driver('two_crash')
enospc = driver('enospc')
altstack = driver('altstack_checkpoint')
links = driver('link_checkpoint')
peers = driver('crash_peer')
pauses = driver('pause_kinds')
storm = driver('output_storm')
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


class DiskRebuildOracleTests(unittest.TestCase):
    def found(self, records, tail=b'', bulk=None):
        return {'records': [list(r) for r in records], 'tail': tail.hex(), 'bulk': bulk or {}}

    def test_acknowledged_records_and_in_flight_continuation_pass(self):
        acked = [(0, i) for i in range(5)]
        found = self.found(acked + [(0, 5), (0, 6)], rebuild.record(0, 7)[:40])
        self.assertEqual(rebuild.verify(found, acked, {}), [])
        self.assertEqual(len(rebuild.record(3, 17)), rebuild.RECORD)

    def test_violations_are_reported(self):
        acked = [(0, i) for i in range(5)]
        cases = [
            self.found(acked[:4]),                                  # Acknowledged record lost.
            self.found(acked + [(0, 9)]),                           # Not a continuation.
            self.found(acked, b'garbage'),                          # Torn tail.
            self.found(acked, bulk={'bulk-00': 'x'}),               # Bulk digest.
        ]
        for found in cases:
            self.assertTrue(rebuild.verify(found, acked, {0: 'y'} if found['bulk'] else {}))

    def test_records_parse_and_stop_at_damage(self):
        data = rebuild.record(1, 0) + rebuild.record(1, 1)
        damaged = bytearray(data)
        damaged[rebuild.RECORD + 20] ^= 1
        self.assertEqual(rebuild.parse(data)[0], [(1, 0), (1, 1)])
        self.assertEqual(rebuild.parse(bytes(damaged))[0], [(1, 0)])


class JournalOracleTests(unittest.TestCase):
    def test_sectors_classify(self):
        self.assertEqual(journal.classify(journal.record(2, 31)), [2, 31])
        self.assertEqual(journal.classify(bytes(journal.SECTOR)), 'zero')
        torn = bytearray(journal.record(1, 3))
        torn[300] ^= 1
        self.assertEqual(journal.classify(bytes(torn)), 'garbage')
        self.assertEqual(journal.valid_prefix([[1, 0], [1, 1], 'zero', [1, 3]]), 2)

    def test_crash_rules(self):
        stable = ['zero'] * 4
        written = {0: ([1, 0], True), 1: ([1, 1], False), 2: ([1, 2], False)}
        kept = [[1, 0], [1, 1], 'zero', 'zero']
        self.assertEqual(journal.check_crash(kept, stable, written, 'torn'), ([], 1, 1))
        self.assertTrue(journal.check_crash(kept, stable, written, 'drop')[0])
        for broken in (['zero', [1, 1], 'zero', 'zero'],      # Flushed record lost.
                       [[1, 0], 'garbage', 'zero', 'zero'],   # Neither new nor stable.
                       [[1, 0], 'zero', 'zero', [9, 9]]):     # Unwritten sector changed.
            self.assertTrue(journal.check_crash(broken, stable, written, 'torn')[0])
        # A survivor the repair overwrote must not come back.
        self.assertTrue(journal.check_crash([[1, 0], [0, 7], 'zero', 'zero'], stable, written, 'torn')[0])


class EnospcOracleTests(unittest.TestCase):
    def outcome(self, **partial):
        return {'fillers': [{'size': 1, 'count': 3}], 'full_write': 'ENOSPC',
                'partial': {'written': 4096, 'short_writes': 1, 'error': 'ENOSPC', 'fsync': 'ok', **partial}}

    def found(self, tail=4096):
        import hashlib
        return {'sentinel-a': hashlib.sha256(enospc.sentinel('a')).hexdigest(),
                'sentinel-b': hashlib.sha256(enospc.sentinel('b')).hexdigest(),
                'tail': {'size': tail, 'intact_prefix': tail}}

    def test_recovered_volume_passes(self):
        self.assertEqual(enospc.verify(self.found(), ('a', 'b'), self.outcome()), [])

    def test_violations_are_reported(self):
        lost = self.found()
        del lost['sentinel-b']
        self.assertTrue(enospc.verify(lost, ('a', 'b'), self.outcome()))
        self.assertTrue(enospc.verify(self.found(1024), ('a', 'b'), self.outcome()))
        self.assertTrue(enospc.verify(self.found(), ('a', 'b'), self.outcome(error='EIO')))


class AltstackOracleTests(unittest.TestCase):
    def result(self, **changes):
        return {'token': altstack.TOKEN, 'read': 4, 'onstack': 1, 'on_altstack': 1,
                'interrupted_on_altstack': 0, 'locals_ok': 1, 'fpcr_ok': 1, 'registers_ok': 1,
                'bad_register': 0, 'blocks': 20, 'blocks_at_release': 12, 'block_errors': 0, **changes}

    def test_intact_handler_passes(self):
        self.assertEqual(altstack.verify(self.result()), [])
        self.assertEqual(altstack.verify(self.result(onstack=0)), [])  # Known translator gap.
        self.assertEqual(len(altstack.known(self.result(onstack=0))), 1)

    def test_violations_are_reported(self):
        for change in ({'registers_ok': 0}, {'fpcr_ok': 0}, {'locals_ok': 0}, {'on_altstack': 0},
                       {'token': 1}, {'block_errors': 1}, {'blocks': 12}, {'interrupted_on_altstack': 1}):
            self.assertTrue(altstack.verify(self.result(**change)), change)


class LinkCheckpointOracleTests(unittest.TestCase):
    def evidence(self, drop_partitioned=True, shaped_spread=900, unshaped_spread=1, lines=50):
        datagrams, bursts, first = [], {}, 0
        spreads = {'queued': 2000, 'partitioned': 0, 'shaped': shaped_spread, 'unshaped': unshaped_spread}
        for name, count in links.BURSTS:
            bursts[name] = (first, count)
            if name != 'partitioned' or not drop_partitioned:
                for i in range(count):
                    datagrams.append((first + i, i * spreads[name] * 1_000_000 // max(count - 1, 1)))
            first += count
        heals = {'drop': {'dropped': 2, 'matched': 6},
                 'shape': {'queue_counts': {'matched': 140, 'queue_dropped': 0, 'delayed_max_ns': 1}}}
        offsets = {'base': 0, 'skewed': links.SKEW_NS, 'healed': 0}
        return bursts, {'datagrams': datagrams, 'lines': list(range(lines))}, heals, offsets

    def test_expected_deliveries_pass(self):
        errors, evidence = links.check(*self.evidence())
        self.assertEqual(errors, [])
        self.assertEqual(evidence['partitioned']['delivered'], 0)

    def test_violations_are_reported(self):
        for case in ({'drop_partitioned': False}, {'shaped_spread': 10}, {'unshaped_spread': 900},
                     {'lines': 3}):
            self.assertTrue(links.check(*self.evidence(**case))[0], case)
        bursts, report, heals, offsets = self.evidence()
        heals['drop']['dropped'] = 0
        self.assertTrue(links.check(bursts, report, heals, offsets)[0])


class CrashPeerOracleTests(unittest.TestCase):
    def outcomes(self, data_ms=10_400, idle_ms=10_200):
        return [{'i': i, 'data': i < peers.DATA, 'error': 'ETIMEDOUT',
                 'ms': data_ms if i < peers.DATA else idle_ms} for i in range(peers.CONNECTIONS)]

    def recovery(self, **changes):
        return {'dormant': 'ECONNRESET', 'closed_port': 'ECONNREFUSED', 'fresh': 'fresh', **changes}

    def test_linux_timer_rules_pass(self):
        witness = {'in_order': True, 'max_gap_ms': 9000, 'echoed': 100}
        self.assertEqual(peers.check(self.outcomes(), self.recovery(), witness, 8.0), [])

    def test_violations_are_reported(self):
        witness = {'in_order': True, 'max_gap_ms': 9000, 'echoed': 100}
        early = self.outcomes(data_ms=9_000)
        disordered = self.outcomes()
        disordered[0]['ms'] = 10_690
        disordered[1]['ms'] = 10_200
        self.assertTrue(peers.check(early, self.recovery(), witness, 8.0))
        self.assertTrue(peers.check(disordered, self.recovery(), witness, 8.0))
        self.assertTrue(peers.check(self.outcomes(), self.recovery(dormant='data'), witness, 8.0))
        self.assertTrue(peers.check(self.outcomes(), self.recovery(closed_port='ETIMEDOUT'), witness, 8.0))
        self.assertTrue(peers.check(self.outcomes(), self.recovery(), dict(witness, max_gap_ms=200), 8.0))


class PauseOracleTests(unittest.TestCase):
    def reports(self, kind, gap_ns=20_100_000_000, retrans=None, burst=200):
        tick = pauses.TICK_NS
        ticks = [(1, i * tick, i * tick) for i in range(10)]
        ticks.append((burst, ticks[-1][1] + gap_ns, ticks[-1][2] + gap_ns))
        echoed = [(n, n * 50_000_000) for n in range(10)]
        echoed += [(10 + n, 21_000_000_000 + n) for n in range(400)]
        if retrans is None:
            retrans = 0 if kind == 'pause_process' else 4
        ping = {'elapsed_ns': 20_010_000_000, 'pause_rpc': [0, 0, 0], 'resume_rpc': [0, 0, 0]}
        return (kind, (0, 20_000_000_000), {'ticks': ticks[:1]}, {'ticks': ticks},
                {'retrans': 10, 'echoed': echoed[:1]}, {'retrans': 10 + retrans, 'echoed': echoed, 'sent': 410},
                ping)

    def test_documented_pauses_pass(self):
        for kind in ('pause_process', 'pause'):
            errors, evidence = pauses.analyse(*self.reports(kind))
            self.assertEqual(errors, [], kind)
            self.assertEqual(evidence['burst'], 200)

    def test_violations_are_reported(self):
        self.assertTrue(pauses.analyse(*self.reports('pause_process', retrans=3))[0])
        self.assertTrue(pauses.analyse(*self.reports('pause', retrans=0))[0])
        self.assertTrue(pauses.analyse(*self.reports('pause', gap_ns=1_000_000_000))[0])
        self.assertTrue(pauses.analyse(*self.reports('pause', burst=1))[0])


class OutputStormFormatTests(unittest.TestCase):
    def test_records_have_exact_sizes_and_one_marker(self):
        pattern = storm.unit('0123abcd', 1, 2)
        for size in (64, 512, 2048, storm.PIPE_BUF, 64 << 10, 1 << 20):
            data = storm.record('0123abcd', 1, 2, 345, size, pattern)
            self.assertEqual(len(data), size)
            self.assertEqual(data.count(0x1e), 1)
        self.assertIn(b'\0', pattern)
        self.assertIn(b'\x1b[', pattern)

    def test_bodies_depend_on_writer_and_sequence(self):
        self.assertNotEqual(storm.record('0123abcd', 1, 2, 3, 64), storm.record('0123abcd', 1, 2, 4, 64))
        self.assertNotEqual(storm.unit('0123abcd', 1, 2), storm.unit('0123abcd', 2, 2))

    def test_full_storm_writes_eight_and_two_mebibytes(self):
        out = err = 0
        plan = storm.FULL
        for thread, (fd, size) in enumerate(storm.ATOMIC):
            for n in range(plan['atomic'][thread]):
                if (fd or 1 + n % 2) == 1:
                    out += size
                else:
                    err += size
        for fd, size, count in plan['large']:
            out, err = (out + size * count, err) if fd == 1 else (out, err + size * count)
        err += plan['bytewise']
        out += sum(storm.ATOMIC[t][1] for t in (1, 2, 3)) * plan['bytewise_atomic']
        self.assertEqual((round(out / 2**20), round(err / 2**20)), (8, 2))


if __name__ == '__main__':
    unittest.main()
