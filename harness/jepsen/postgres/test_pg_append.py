"""Unit tests for the list-append workload pieces that need no server: python3 -m unittest test_pg_append.py"""
import io
import json
import random
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "elle"))
import pg_append  # noqa: E402


class Generator(unittest.TestCase):
    def test_values_are_unique_per_key_and_keys_retire(self):
        keys = pg_append.Keys(random.Random(7), key_count=10, max_writes=32, max_length=4, read_fraction=0.5)
        seen, lengths = set(), set()
        for _ in range(2000):
            ops = keys.transaction()
            lengths.add(len(ops))
            for f, k, v in ops:
                if f == "append":
                    self.assertNotIn((k, v), seen)
                    self.assertLessEqual(v, 32)
                    seen.add((k, v))
        self.assertEqual(lengths, {1, 2, 3, 4})
        self.assertGreater(keys.next_key, 10, "hot keys must be retired for fresh ones")

    def test_parse_reads(self):
        self.assertEqual(pg_append.parse([]), [])
        self.assertEqual(pg_append.parse([[None]]), [])
        self.assertEqual(pg_append.parse([["3,1,2"]]), [3, 1, 2])


class Recheck(unittest.TestCase):
    def test_saved_history_rechecks(self):
        lines = [
            {"i": 0, "p": 0, "type": "ok", "t0": 0, "t1": 1, "value": [["r", 1, []], ["append", 2, 1]], "error": None},
            {"i": 1, "p": 1, "type": "ok", "t0": 0, "t1": 1, "value": [["r", 2, []], ["append", 1, 1]], "error": None},
            {"i": 2, "p": 2, "type": "fail", "t0": 0, "t1": 1, "value": [["append", 1, 2]], "error": "40001"},
        ]
        with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as handle:
            handle.write("EVIDENCE {}\n" + "".join("H " + json.dumps(line) + "\n" for line in lines))
        txns = pg_append.load(handle.name)
        self.assertEqual([t.type for t in txns], ["ok", "ok", "fail"])
        with redirect_stdout(io.StringIO()) as out:
            result = pg_append.report(txns, "serializable", 5)
        self.assertEqual(result["counts"], {"G2-item": 1})
        self.assertIn("ANOMALY G2-item", out.getvalue())


if __name__ == "__main__":
    unittest.main()
