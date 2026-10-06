#!/usr/bin/env python3
"""Mocked cleanup failure controls; never reads or writes a hardware MSR."""
import contextlib,importlib.util,io,json,os,runpy,signal,struct,subprocess,sys,tempfile,unittest
from pathlib import Path
from unittest.mock import Mock,patch
here=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('guard',here/'run_guard.py');guard=importlib.util.module_from_spec(spec);spec.loader.exec_module(guard)
class CleanupTests(unittest.TestCase):
 def test_group_exited_between_poll_and_signal(self):
  child=Mock(pid=12345);child.poll.return_value=None
  with patch.object(guard.os,'killpg',side_effect=ProcessLookupError):guard.stop_owned_group(child)
  child.wait.assert_called_once_with()
 def test_timeout_escalates_only_owned_group(self):
  child=Mock(pid=12345);child.poll.return_value=None
  child.wait.side_effect=[subprocess.TimeoutExpired('owned',2),0]
  with patch.object(guard.os,'killpg')as kill:guard.stop_owned_group(child)
  self.assertEqual(kill.call_args_list[0].args,(12345,signal.SIGTERM))
  self.assertEqual(kill.call_args_list[1].args,(12345,signal.SIGKILL))
  self.assertEqual(child.wait.call_count,2)
 def test_restore_attempts_both_after_first_failure(self):
  with tempfile.TemporaryDirectory()as tmp:
   snap=Path(tmp)/'snapshot.json';snap.write_text(json.dumps({'14':100,'30':200}));writes=[]
   def write(fd,data,offset):
    writes.append(fd)
    if fd==14:raise OSError('mock first CPU failure')
    return 8
   def read(fd,count,offset):return struct.pack('<Q',100+(1<<54)if fd==14 else 200)
   with patch.object(sys,'argv',['msr.py','restore',str(snap),'14,30']),patch.object(os,'open',side_effect=lambda name,flags:int(name.split('/')[3])),patch.object(os,'close'),patch.object(os,'pwrite',side_effect=write),patch.object(os,'pread',side_effect=read),contextlib.redirect_stdout(io.StringIO()):
    with self.assertRaises(SystemExit)as fail:runpy.run_path(str(here/'msr.py'),run_name='__main__')
   self.assertEqual(fail.exception.code,1);self.assertEqual(writes,[14,30]);self.assertTrue(snap.exists())
if __name__=='__main__':unittest.main()
