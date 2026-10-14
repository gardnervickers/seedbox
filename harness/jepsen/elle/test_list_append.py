"""Hand-built list-append histories for the Elle-style checker: python3 -m unittest test_list_append.py"""
import unittest

from list_append import Txn, check, from_history


def ok(id, *ops):
    return Txn(id, "ok", ops)


def a(k, v):
    return ["append", k, v]


def r(k, value):
    return ["r", k, value]


class Serial(unittest.TestCase):
    def test_serial_history_is_valid(self):
        result = check([
            ok(1, a("x", 1), r("y", [])),
            ok(2, r("x", [1]), a("y", 2)),
            ok(3, a("x", 3), r("x", [1, 3])),
            ok(4, r("x", [1, 3]), r("y", [2])),
        ])
        self.assertTrue(result["valid"], result)
        self.assertEqual(result["anomaly-types"], [])
        self.assertEqual(result["edges"], {"ww": 1, "wr": 4, "rw": 2})

    def test_concurrent_but_serializable_reads(self):
        # T2 reads before T1's append; T3 after. Order T2, T1, T3 explains it.
        result = check([ok(1, a("x", 1)), ok(2, r("x", [])), ok(3, r("x", [1]))])
        self.assertTrue(result["valid"], result)

    def test_aborted_and_unobserved_indeterminate_writes_are_ignored(self):
        result = check([
            ok(1, a("x", 1)),
            Txn(2, "fail", [a("x", 2)]),
            Txn(3, "info", [a("x", 3), r("y", None)]),
            ok(4, r("x", [1])),
        ])
        self.assertTrue(result["valid"], result)


class Cycles(unittest.TestCase):
    def assertOnly(self, result, kinds):
        self.assertEqual(result["anomaly-types"], kinds, result)
        self.assertFalse(result["valid"])

    def test_g0_write_cycle(self):
        result = check([
            ok(1, a("x", 1), a("y", 1)),
            ok(2, a("x", 2), a("y", 2)),
            ok(3, r("x", [1, 2]), r("y", [2, 1])),
        ])
        self.assertIn("G0", result["anomaly-types"])
        cycle = result["anomalies"]["G0"][0]
        self.assertEqual(sorted(cycle["cycle"]), [1, 2])
        self.assertTrue(all(step["type"] == "ww" for step in cycle["steps"]))

    def test_g1c_circular_information_flow(self):
        result = check([ok(1, a("x", 1), r("y", [2])), ok(2, a("y", 2), r("x", [1]))])
        self.assertOnly(result, ["G1c"])

    def test_g_single_read_skew(self):
        # T1 sees T2's y but not T2's x.
        result = check([
            ok(1, r("x", []), r("y", [1])),
            ok(2, a("x", 1), a("y", 1)),
            ok(3, r("x", [1])),
        ])
        self.assertOnly(result, ["G-single"])
        explanation = result["anomalies"]["G-single"][0]["explanation"]
        self.assertIn("did not observe", explanation)
        self.assertIn("a contradiction!", explanation)

    def test_g2_item_write_skew(self):
        history = [
            ok(1, r("x", []), a("y", 1)),
            ok(2, r("y", []), a("x", 1)),
            ok(3, r("x", [1]), r("y", [1])),
        ]
        result = check(history)
        self.assertOnly(result, ["G2-item"])
        steps = result["anomalies"]["G2-item"][0]["steps"]
        self.assertEqual([step["type"] for step in steps], ["rw", "rw"])
        # Snapshot isolation allows write skew.
        self.assertTrue(check(history, model="snapshot-isolation")["valid"])

    def test_g2_item_from_unobserved_appends(self):
        # Nobody reads the final state: the anti-dependencies come from unobserved committed appends.
        result = check([ok(1, r("x", []), a("y", 1)), ok(2, r("y", []), a("x", 1))])
        self.assertOnly(result, ["G2-item"])

    def test_read_only_anomaly_postgres_shape(self):
        # Jepsen's PostgreSQL 12.3 trio: the read-only T3 sees T2's insert of key 1670
        # but not T1's insert of 1671, while T1 missed T2's 1670.
        result = check([
            ok(1, r(1670, []), a(1671, 1)),
            ok(2, a(1670, 1)),
            ok(3, r(1670, [1]), r(1671, [])),
            ok(4, r(1670, [1]), r(1671, [1])),
        ])
        self.assertOnly(result, ["G2-item"])
        cycle = result["anomalies"]["G2-item"][0]
        self.assertEqual(sorted(cycle["cycle"]), [1, 2, 3])
        self.assertEqual(sorted(step["type"] for step in cycle["steps"]), ["rw", "rw", "wr"])

    def test_observed_indeterminate_write_counts_as_committed(self):
        result = check([
            ok(1, r("x", []), a("y", 1)),
            Txn(2, "info", [r("y", None), a("x", 1)]),
            ok(3, r("x", [1]), r("y", [1])),
        ])
        # T2's reads are unknown, so only T1 -> T2 (rw on x) is known: no cycle.
        self.assertTrue(result["valid"], result)
        self.assertEqual(result["edges"], {"rw": 1, "wr": 2})

    def test_indeterminate_write_closes_a_cycle_when_observed(self):
        result = check([
            ok(1, a("x", 1), r("y", [2])),
            Txn(2, "info", [a("y", 2), a("z", 2)]),
            ok(3, r("x", [1]), r("z", [])),
            ok(4, a("z", 4)),
            ok(5, r("z", [4])),
        ])
        # T2 committed (T1 saw its y); T1 -wr-> T3, T3 -rw-> T2 (missed z 2), T2 -wr-> T1.
        self.assertIn("G-single", result["anomaly-types"], result)


class NonCyclic(unittest.TestCase):
    def test_g1a_aborted_read(self):
        result = check([Txn(1, "fail", [a("x", 1)]), ok(2, r("x", [1]))])
        self.assertEqual(result["anomaly-types"], ["G1a"])
        self.assertFalse(result["valid"])

    def test_g1b_intermediate_read(self):
        result = check([ok(1, a("x", 1), a("x", 2)), ok(2, r("x", [1]))])
        self.assertIn("G1b", result["anomaly-types"])

    def test_own_intermediate_read_is_fine(self):
        result = check([ok(1, a("x", 1), r("x", [1]), a("x", 2)), ok(2, r("x", [1, 2]))])
        self.assertTrue(result["valid"], result)

    def test_internal_inconsistency(self):
        result = check([ok(1, r("x", []), a("x", 1), r("x", []))])
        self.assertEqual(result["anomaly-types"], ["internal"])
        result = check([ok(1, a("x", 1), r("x", [5]))])
        self.assertIn("internal", result["anomaly-types"])

    def test_incompatible_order(self):
        result = check([ok(1, a("x", 1)), ok(2, a("x", 2)), ok(3, r("x", [1, 2])), ok(4, r("x", [2, 1]))])
        self.assertIn("incompatible-order", result["anomaly-types"])

    def test_duplicates_and_garbage(self):
        result = check([ok(1, a("x", 1)), ok(2, r("x", [1, 1, 9]))])
        self.assertIn("duplicate-elements", result["anomaly-types"])
        self.assertIn("garbage", result["anomaly-types"])

    def test_duplicate_append_is_a_generator_error(self):
        with self.assertRaises(ValueError):
            check([ok(1, a("x", 1)), ok(2, a("x", 1))])


class History(unittest.TestCase):
    def test_pairs_invocations_and_completions(self):
        txns = from_history([
            {"process": 0, "type": "invoke", "value": [a("x", 1), r("y", None)], "time": 1},
            {"process": 1, "type": "invoke", "value": [r("x", None)], "time": 2},
            {"process": 0, "type": "ok", "value": [a("x", 1), r("y", [])], "time": 3},
            {"process": 1, "type": "info", "value": None, "time": 9},
            {"process": 2, "type": "invoke", "value": [a("y", 3)], "time": 10},
        ])
        self.assertEqual([(t.type, t.process) for t in txns], [("ok", 0), ("info", 1), ("info", 2)])
        self.assertEqual(txns[1].value, [r("x", None)])
        self.assertEqual((txns[0].invoke, txns[0].complete), (1, 3))


def simulate(seed, isolation, clients=6, txns=300, keys=4):
    """An in-memory list-append database: ``serial`` runs each transaction alone;
    ``snapshot`` interleaves them with snapshot reads and first-committer-wins."""
    import random
    rng = random.Random(seed)
    db, version, out, counter = {}, {}, [], [0]
    active = {}  # client -> [txn ops, position, snapshot, writes, start version]
    done = 0
    clock = [0]
    while done < txns or active:
        client = rng.randrange(clients)
        if client not in active:
            if done >= txns:
                continue
            done += 1
            ops = []
            for _ in range(rng.randint(1, 4)):
                k = rng.randrange(keys)
                if rng.random() < 0.5:
                    ops.append(["r", k, None])
                else:
                    counter[0] += 1
                    ops.append(["append", k, counter[0]])
            active[client] = [ops, 0, {k: list(v) for k, v in db.items()}, {}, dict(version), len(out)]
        state = active[client]
        ops, _, snapshot, writes, start, _ = state
        # A serial transaction runs every step at once; a snapshot one, one step per turn.
        for _ in range(len(ops) if isolation == "serial" else 1):
            if state[1] < len(ops):
                f, k, v = ops[state[1]]
                if f == "r":
                    ops[state[1]] = ["r", k, list(snapshot.get(k, [])) + writes.get(k, [])]
                else:
                    writes.setdefault(k, []).append(v)
                state[1] += 1
                if isolation != "serial":
                    break
        if state[1] < len(ops) or (isolation != "serial" and rng.random() < 0.5):
            continue
        del active[client]
        clock[0] += 1
        if any(version.get(k, 0) != start.get(k, 0) for k in writes):
            out.append(Txn(clock[0], "fail", ops))
            continue
        for k, vs in writes.items():
            db.setdefault(k, []).extend(vs)
            version[k] = version.get(k, 0) + 1
        out.append(Txn(clock[0], "ok", ops))
    final = [["r", k, list(v)] for k, v in sorted(db.items())]
    out.append(Txn(clock[0] + 1, "ok", final))
    return out


class Executors(unittest.TestCase):
    def test_serial_executor_is_always_valid(self):
        for seed in range(20):
            result = check(simulate(seed, "serial"))
            self.assertTrue(result["valid"], (seed, result["counts"]))

    def test_snapshot_executor_shows_only_g2_item(self):
        g2 = 0
        for seed in range(20):
            result = check(simulate(seed, "snapshot"), model="snapshot-isolation")
            self.assertTrue(result["valid"], (seed, result["counts"]))
            g2 += result["counts"].get("G2-item", 0)
        self.assertGreater(g2, 0)


if __name__ == "__main__":
    unittest.main()
