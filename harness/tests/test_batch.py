"""Lifecycle controls use real disposable processes, never hardware MSRs."""
import fcntl
import importlib.util
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

SIM = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SIM))
spec = importlib.util.spec_from_file_location('batch', SIM/'batch.py')
batch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(batch)


class BatchTests(unittest.TestCase):
    def test_cpu_ranges(self):
        self.assertEqual(batch.cpu_list('14,30-31'), {14,30,31})

    def test_overlap_lock_fails_and_releases_acquired_locks(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            with batch.cpu_locks({30}, directory):
                with self.assertRaises(RuntimeError):
                    with batch.cpu_locks({14,30}, directory):
                        self.fail('overlap was accepted')
                with batch.cpu_locks({14}, directory):
                    pass
            with batch.cpu_locks({14,30}, directory):
                pass

    def test_invalid_timeout_is_rejected_before_mutation(self):
        for timeout in (0, -1, float("nan"), float("inf")):
            with self.assertRaises(ValueError):
                batch.run_batch(Path("."),"world","control",[1],[],[14],timeout,"unused","adaptive")

    def exercise_lifecycle(self, interrupt=False, count=1):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            children = []
            spawned = threading.Event()
            real_popen = subprocess.Popen
            def launch(*args, **kwargs):
                children.append(real_popen([sys.executable, '-c', 'import time; time.sleep(60)'],
                                           **kwargs))
                if len(children) == count:
                    spawned.set()
                return children[-1]
            def set_msr(cpus, snapshot):
                snapshot.write_text('{}')
            restored = []
            def restore(cpus, snapshot):
                self.assertTrue(spawned.is_set())
                self.assertTrue(all(child.returncode is not None for child in children))
                for child in children:
                    with self.assertRaises(ProcessLookupError):
                        os.killpg(child.pid, 0)
                for cpu in cpus:
                    with self.assertRaises(RuntimeError):
                        with batch.cpu_locks({cpu}, root):
                            pass
                restored.append(True)
                snapshot.unlink()
            sender = None
            if interrupt:
                def signal_after_start():
                    if spawned.wait(2):
                        os.kill(os.getpid(), signal.SIGTERM)
                sender = threading.Thread(target=signal_after_start)
                sender.start()
            with patch.object(batch, 'siblings', side_effect=lambda cpu:{cpu,cpu+16}), \
                 patch.object(batch.subprocess, 'Popen', side_effect=launch), \
                 patch.object(batch.msr54, 'required', return_value=True), \
                 patch.object(batch.msr54, 'show'), \
                 patch.object(batch.msr54, 'set_workaround', side_effect=set_msr), \
                 patch.object(batch.msr54, 'restore_snapshot', side_effect=restore):
                result = batch.run_batch(root,'world.json','control',list(range(count)),[],
                                         list(range(14,14+count)),.08,'unused','adaptive',root)
            if sender:
                sender.join(2)
            self.assertEqual(restored, [True])
            self.assertEqual(result,143 if interrupt else 0)
            for i in range(count):
                self.assertEqual((root/f'runs/control-s{i}-r{i}.status').read_text().strip(),
                                 '143' if interrupt else '124')
            with batch.cpu_locks({14,30}, root):
                pass

    @unittest.skipUnless(sys.platform == 'linux' and hasattr(os,"WNOWAIT"),
                         "Linux waitid lifecycle control")
    def test_timeout_reaps_before_restore_and_preserves_cpu_locks(self):
        self.exercise_lifecycle()

    @unittest.skipUnless(sys.platform == 'linux' and hasattr(os,"WNOWAIT"),
                         "Linux waitid lifecycle control")
    def test_interruption_reaps_every_concurrent_child_before_restore(self):
        self.exercise_lifecycle(interrupt=True,count=2)

    @unittest.skipUnless(sys.platform == 'linux', 'Linux subreaper control')
    def test_orphan_descendant_is_reaped_before_return(self):
        batch.enable_subreaper()
        with tempfile.TemporaryDirectory() as tmp:
            pid_file = Path(tmp)/'descendant'
            child = subprocess.Popen([sys.executable,'-c',
                'import os,signal,time,sys; p=os.fork(); '
                # Publish the pid atomically so the wait below never reads an empty file.
                'open(sys.argv[1]+".tmp","w").write(str(p)) if p else None; '
                'os.replace(sys.argv[1]+".tmp",sys.argv[1]) if p else None; '
                'signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(60)',str(pid_file)],
                start_new_session=True)
            try:
                deadline = time.monotonic()+2
                while not pid_file.exists() and time.monotonic()<deadline:
                    time.sleep(.01)
                descendant = int(pid_file.read_text())
                os.kill(child.pid,signal.SIGKILL)
                deadline = time.monotonic()+2
                while batch.exit_status(child) is None and time.monotonic()<deadline:
                    time.sleep(.01)
                batch.stop_owned_group(child)
                with self.assertRaises(ProcessLookupError):
                    os.kill(descendant,0)
                with self.assertRaises(ProcessLookupError):
                    os.killpg(child.pid,0)
            finally:
                if child.returncode is None:
                    batch.stop_owned_group(child)


if __name__ == '__main__':
    unittest.main()
