"""Supervisor control-flow tests; no root or cgroup delegation required."""
import contextlib
import io
import os
from pathlib import Path
import signal
import tempfile
import unittest
from unittest.mock import patch

import cgroup


class SupervisorTests(unittest.TestCase):
    def setUp(self):
        self.handlers = {n: signal.getsignal(n) for n in (
            signal.SIGTERM, signal.SIGINT, signal.SIGHUP
        )}
        self.mask = signal.pthread_sigmask(signal.SIG_BLOCK, set())
        self.argv = ["cgroup.py", "--uid", "1000", "--gid", "1000", "--", "fixture"]

    def tearDown(self):
        for number, handler in self.handlers.items():
            signal.signal(number, handler)
        signal.pthread_sigmask(signal.SIG_SETMASK, self.mask)

    def main(self):
        stderr = io.StringIO()
        with patch("sys.argv", self.argv), contextlib.redirect_stderr(stderr):
            result = cgroup.main()
        return result, stderr.getvalue()

    def test_cancellation_cleans_owned_domain_and_preserves_signal_status(self):
        # Exercise launch's actual finally block with simulated kernel cgroup
        # files and child bookkeeping. Never move or signal a real child.
        for number in self.handlers:
            with self.subTest(signal=number), tempfile.TemporaryDirectory() as directory:
                signal.pthread_sigmask(signal.SIG_SETMASK, self.mask)
                mount = Path(directory)
                (mount / "cgroup.subtree_control").write_text("pids")
                membership = mount / "membership"
                membership.write_text("0::/\n")
                real_path = Path
                real_exists = Path.exists
                real_kill = os.kill
                calls = []

                def path(value):
                    return {"/sys/fs/cgroup": mount,
                            "/proc/self/cgroup": membership}.get(value, real_path(value))

                def waitpid(pid, flags):
                    calls.append(pid)
                    if len(calls) == 1:
                        real_kill(os.getpid(), number)
                        self.fail("cancellation handler did not interrupt wait")
                    if pid == -1:
                        raise ChildProcessError()
                    return pid, 0

                with patch.object(cgroup, "Path", side_effect=path), \
                        patch.object(cgroup.os, "geteuid", return_value=0), \
                        patch.object(cgroup.ctypes, "CDLL") as libc, \
                        patch.object(cgroup.os, "chown"), \
                        patch.object(cgroup.os, "fork", return_value=12345), \
                        patch.object(cgroup.os, "write", return_value=1), \
                        patch.object(cgroup.os, "kill") as kill, \
                        patch.object(cgroup.os, "waitpid", side_effect=waitpid), \
                        patch.object(cgroup, "wait_empty") as empty, \
                        patch.object(Path, "exists", lambda p: p.name == "cgroup.kill" or real_exists(p)), \
                        patch.object(Path, "rmdir") as rmdir:
                    libc.return_value.prctl.return_value = 0
                    result, stderr = self.main()
                root = next(mount.glob("seedbox-dr-*"))
                self.assertEqual(result, 128 + number)
                self.assertIn("isolate: cancelled:", stderr)
                self.assertNotIn("setup cgroup delegation", stderr)
                kill.assert_called_once_with(12345, signal.SIGKILL)
                self.assertEqual((root / "cgroup.kill").read_text(), "1")
                empty.assert_called_once_with(root)
                self.assertEqual(calls, [12345, 12345, -1])
                self.assertEqual(rmdir.call_count, 2)  # controller then owned root

    def test_missing_parent_pids_is_a_setup_error_before_owning_resources(self):
        with tempfile.TemporaryDirectory() as directory:
            mount = Path(directory)
            (mount / "cgroup.subtree_control").write_text("")
            membership = mount / "membership"
            membership.write_text("0::/\n")
            real_path = Path

            def path(value):
                return {"/sys/fs/cgroup": mount,
                        "/proc/self/cgroup": membership}.get(value, real_path(value))

            with patch.object(cgroup, "Path", side_effect=path), \
                    patch.object(cgroup.os, "geteuid", return_value=0):
                result, stderr = self.main()
            self.assertEqual(result, 2)
            self.assertIn("parent has not delegated pids", stderr)
            self.assertEqual(list(mount.glob("seedbox-dr-*")), [])

    def test_setup_errors_remain_setup_errors(self):
        for error in (OSError("denied"), RuntimeError("missing pids"),
                      InterruptedError("ordinary interrupted syscall")):
            with self.subTest(error=error), patch.object(cgroup, "launch", side_effect=error):
                result, stderr = self.main()
                self.assertEqual(result, 2)
                self.assertIn("isolate: setup cgroup delegation:", stderr)

    def test_child_status_is_preserved(self):
        for status, expected in ((0, 0), (7, 7), (-signal.SIGTERM, 143)):
            with self.subTest(status=status), patch.object(cgroup, "launch", return_value=status):
                self.assertEqual(self.main(), (expected, ""))


if __name__ == "__main__":
    unittest.main()
