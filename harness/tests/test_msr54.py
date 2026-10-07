import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('msr54',Path(__file__).resolve().parents[1]/'msr54.py')
msr=importlib.util.module_from_spec(spec);spec.loader.exec_module(msr)

class MsrTests(unittest.TestCase):
 def test_only_zen_requires_the_workaround(self):
  with tempfile.TemporaryDirectory() as directory:
   cpuinfo=Path(directory)/'cpuinfo'
   for vendor,family,expected in [('AuthenticAMD',25,True),('AuthenticAMD',23,True),('AuthenticAMD',21,False),('GenuineIntel',6,False)]:
    cpuinfo.write_text(f'processor\t: 0\nvendor_id\t: {vendor}\ncpu family\t: {family}\n\nprocessor\t: 1\nvendor_id\t: {vendor}\n')
    self.assertEqual(msr.required(cpuinfo),expected)
 def test_restore_attempts_both_cpus_retains_snapshot_on_error(self):
  with tempfile.TemporaryDirectory() as directory:
   snapshot=Path(directory)/'saved.json';snapshot.write_text('{}')
   writes=[]
   def wr(cpu,value):
    writes.append(cpu)
    if cpu==14:raise OSError('injected')
   with patch.object(msr,'wr',wr),patch.object(msr,'rd',lambda cpu:7):
    with self.assertRaises(OSError):msr.restore({14:7,30:7},snapshot)
   self.assertEqual(writes,[14,30]);self.assertTrue(snapshot.exists())
 def test_restore_requires_exact_readback(self):
  with tempfile.TemporaryDirectory() as directory:
   snapshot=Path(directory)/'saved.json';snapshot.touch()
   with patch.object(msr,'wr'),patch.object(msr,'rd',return_value=8):
    with self.assertRaises(OSError):msr.restore({14:7,30:7},snapshot)
   self.assertTrue(snapshot.exists())
 def test_restore_removes_only_verified_snapshot(self):
  with tempfile.TemporaryDirectory() as directory:
   snapshot=Path(directory)/'saved.json';snapshot.touch()
   with patch.object(msr,'wr'),patch.object(msr,'rd',return_value=7):msr.restore({14:7,30:7},snapshot)
   self.assertFalse(snapshot.exists())

 def test_partial_set_failure_rolls_back_every_cpu(self):
  with tempfile.TemporaryDirectory() as directory:
   snapshot=Path(directory)/'saved.json';values={14:7,30:9};writes=[]
   def wr(cpu,value):
    writes.append((cpu,value))
    if cpu==30 and value & msr.BIT:raise OSError('injected set failure')
    values[cpu]=value
   with patch.object(msr,'wr',wr),patch.object(msr,'rd',side_effect=lambda cpu:values[cpu]):
    with self.assertRaises(OSError):msr.set_workaround([14,30],snapshot)
   self.assertEqual(values,{14:7,30:9});self.assertFalse(snapshot.exists())
   self.assertEqual(writes[-2:],[(14,7),(30,9)])

 def test_exclusive_snapshot_prevents_any_write(self):
  with tempfile.TemporaryDirectory() as directory:
   snapshot=Path(directory)/'saved.json';snapshot.write_text('preserve')
   with patch.object(msr,'rd',return_value=7),patch.object(msr,'wr') as write:
    with self.assertRaises(FileExistsError):msr.set_workaround([14,30],snapshot)
   write.assert_not_called();self.assertEqual(snapshot.read_text(),'preserve')
