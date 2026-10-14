"""Checker tests for etcd_jepsen.py; run with `python3 -m unittest` in this directory."""
import unittest

from etcd_jepsen import check


def history(*steps):
    """steps: (client, op, type, extra) tuples, numbered in order."""
    entries = []
    for client, op, kind, extra in steps:
        entries.append(dict(extra, client=client, op=op, type=kind, i=len(entries)))
    return entries


def rmw(client, op, key, element, granted=10):
    return [(client, op, "acquired", {"key": key, "granted_revision": granted, "key_present": True}),
            (client, None, "read", {"key": key, "ok": True}),
            (client, None, "write", {"key": key, "ok": True, "element": element}),
            (client, op, "ok", {"element": element}),
            (client, op, "releasing", {"key": key})]


class CheckTest(unittest.TestCase):
    def test_serial_holds_lose_nothing(self):
        h = history(*rmw("c1", 1, "lock/a", "c1-1"), *rmw("c2", 1, "lock/b", "c2-1"))
        result, details, violation = check(h, ["c1-1", "c2-1"], "none", [])
        self.assertFalse(violation)
        self.assertEqual((result["acked"], result["lost"], result["overlap_pairs"]), (2, 0, 0))

    def test_paused_holder_overwrites_the_next_holder(self):
        a, b = rmw("c1", 1, "lock/a", "c1-1"), rmw("c2", 1, "lock/b", "c2-1")
        # c1 reads, is paused; c2 acquires, reads, writes; c1 writes its stale set.
        h = history(a[0], a[1], *b[:4], a[2], a[3], a[4], b[4])
        result, details, violation = check(h, ["c1-1"], "none", [])
        self.assertTrue(violation)
        self.assertEqual((result["lost"], result["overlap_pairs"], result["interleaved_rmw"]), (1, 1, 1))
        self.assertEqual(details["lost_elements"], ["c2-1"])

    def test_guarded_overlap_without_loss_is_not_a_violation(self):
        a, b = rmw("c1", 1, "lock/a", "c1-1"), rmw("c2", 1, "lock/b", "c2-1")
        fenced = ("c1", None, "write", {"key": "lock/a", "ok": False, "fenced": True, "element": "c1-1"})
        h = history(a[0], a[1], *b[:4], fenced, a[4], b[4])
        result, _, violation = check(h, ["c2-1"], "token", [])
        self.assertFalse(violation)
        self.assertEqual((result["lost"], result["overlap_pairs"], result["fenced"]), (0, 1, 1))

    def test_grant_after_key_deletion_is_stale(self):
        h = history(*rmw("c1", 1, "lock/a", "c1-1", granted=12))
        events = [{"type": "PUT", "key": "lock/a", "mod_revision": 5, "create_revision": 5},
                  {"type": "DELETE", "key": "lock/a", "mod_revision": 9, "create_revision": 0}]
        result, details, _ = check(h, ["c1-1"], "none", events)
        self.assertEqual(result["stale_grants"], 1)
        self.assertEqual(details["stale"][0]["deleted_revision"], 9)


if __name__ == "__main__":
    unittest.main()
