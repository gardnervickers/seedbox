"""Focused native-owner tests; no language/runtime workload is executed."""
import contextlib
import io
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import phase


class PhaseOutput(unittest.TestCase):
    def test_success_failure_and_overflow(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for code in (0, 7):
                report = io.StringIO()
                with contextlib.redirect_stdout(report):
                    status, out, err = phase.execute(
                        ROOT, [sys.executable, "-c",
                               f"import sys; print('child-out'); print('child-err', file=sys.stderr); sys.exit({code})"],
                        directory, {}, timeout=30)
                self.assertEqual(status, code)
                self.assertEqual(out, "child-out\n")
                self.assertEqual(err, "child-err\n")
                self.assertIn("child-out", report.getvalue())
                self.assertIn("child-err", report.getvalue())
            with patch.object(phase, "LIMIT", 16), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "output exceeded"):
                    phase.execute(ROOT, [sys.executable, "-c", "print('x' * 100)"], directory, {}, timeout=30)

    def test_timeout_retains_output_and_reaps_child(self):
        with tempfile.TemporaryDirectory() as temporary, socket.socket(socket.AF_UNIX) as ready:
            directory = Path(temporary)
            endpoint = str(directory / "ready")
            ready.bind(endpoint)
            ready.listen()
            ready.settimeout(30)
            identity = []
            original = subprocess.Popen

            class ReadyProcess(original):
                first = True

                def wait(self, timeout=None):
                    if self.first:
                        self.first = False
                        with ready.accept()[0] as stream:
                            stream.settimeout(30)
                            pid = int(stream.recv(64))
                            identity.append(os.pidfd_open(pid))
                            # Child is now blocked on our socket. Inject an
                            # immediate deadline only after its positive event.
                            return super().wait(timeout=0)
                    return super().wait(timeout=timeout)

            command = [sys.executable, "-c", """
import os, socket, sys
print('timeout-out', flush=True)
print('timeout-err', file=sys.stderr, flush=True)
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.sendall(str(os.getpid()).encode())
s.recv(1)
# EOF also stays alive until the native owner kills/reaps us.
import signal
signal.pause()
""", endpoint]
            report = io.StringIO()
            try:
                with patch.object(phase.subprocess, "Popen", ReadyProcess), contextlib.redirect_stdout(report):
                    with self.assertRaises(subprocess.TimeoutExpired):
                        phase.execute(ROOT, command, directory, {})
                self.assertIn("timeout-out", report.getvalue())
                self.assertIn("timeout-err", report.getvalue())
                self.assertEqual(len(identity), 1)
                poll = select.poll()
                poll.register(identity[0], select.POLLIN)
                events = poll.poll(30000)
                self.assertTrue(events)
                self.assertTrue(events[0][1] & select.POLLHUP)
            finally:
                for fd in identity:
                    os.close(fd)


if __name__ == "__main__":
    ROOT = Path(sys.argv.pop(1)).resolve()
    unittest.main()
