"""Counter supervisor controls use real processes and locks, with fake MSRs."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
spec = importlib.util.spec_from_file_location("check_counter", HARNESS / "check-counter.py")
counter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(counter)


@unittest.skipUnless(sys.platform == "linux", "Linux affinity and process supervision")
class CounterLifecycleTests(unittest.TestCase):
    def exercise(self, mode):
        with tempfile.TemporaryDirectory() as directory, \
             contextlib.redirect_stdout(io.StringIO()):
            root = Path(directory)
            pidfile = root / "descendant"
            cpu = min(os.sched_getaffinity(0))
            children, restored, snapshots = [], [], []
            real_popen = subprocess.Popen
            real_locks = counter.batch.cpu_locks

            def launch(*args, **kwargs):
                child = real_popen(*args, **kwargs)
                children.append(child)
                return child

            def set_msr(cpus, snapshot):
                snapshots.append(snapshot)
                snapshot.write_text("{}")

            def restore(cpus, snapshot):
                self.assertTrue(children)
                self.assertTrue(all(child.returncode is not None for child in children))
                for child in children:
                    with self.assertRaises(ProcessLookupError):
                        os.killpg(child.pid, 0)
                descendant = int(pidfile.read_text())
                with self.assertRaises(ProcessLookupError):
                    os.kill(descendant, 0)
                with self.assertRaises(RuntimeError):
                    with real_locks(cpus, root):
                        pass
                restored.append(True)
                snapshot.unlink()

            code = """
import os, signal, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
pid = os.fork()
if pid:
    with open(sys.argv[1] + '.tmp', 'w') as stream:
        stream.write(str(pid))
    os.replace(sys.argv[1] + '.tmp', sys.argv[1])
    if sys.argv[2] == 'failure':
        sys.exit(7)
time.sleep(60)
"""
            sender = None
            if mode == "interrupt":
                def interrupt():
                    deadline = time.monotonic() + 5
                    while not pidfile.exists() and time.monotonic() < deadline:
                        time.sleep(.01)
                    if pidfile.exists():
                        os.kill(os.getpid(), signal.SIGTERM)
                sender = threading.Thread(target=interrupt)
                sender.start()
            try:
                with patch.object(counter.subprocess, "Popen", side_effect=launch), \
                     patch.object(counter.batch, "cpu_locks", side_effect=lambda cpus: real_locks(cpus, root)), \
                     patch.object(counter.msr54, "required", return_value=True), \
                     patch.object(counter.msr54, "set_workaround", side_effect=set_msr), \
                     patch.object(counter.msr54, "restore_snapshot", side_effect=restore):
                    command = [sys.executable, "-c", code, str(pidfile), mode]
                    if mode == "failure":
                        self.assertEqual(counter.run_leased(command, cpu, 5), 7)
                    elif mode == "timeout":
                        with self.assertRaises(TimeoutError):
                            counter.run_leased(command, cpu, 1)
                    else:
                        with self.assertRaises(counter.batch.Interrupted):
                            counter.run_leased(command, cpu, 5)
            finally:
                if sender:
                    sender.join(6)
            self.assertEqual(restored, [True])
            self.assertTrue(all(not path.parent.exists() for path in snapshots))
            with real_locks(counter.batch.siblings(cpu), root):
                pass

    def test_failure_reaps_orphan_before_restoring_under_lock(self):
        self.exercise("failure")

    def test_timeout_reaps_every_process_before_restoring_under_lock(self):
        self.exercise("timeout")

    def test_signal_reaps_every_process_before_restoring_under_lock(self):
        self.exercise("interrupt")


if __name__ == "__main__":
    unittest.main()
