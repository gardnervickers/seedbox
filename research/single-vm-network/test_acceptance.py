#!/usr/bin/env python3
"""Reject misleading pass markers on process, world or guest exit failures."""
import json,subprocess,sys,tempfile,unittest
from pathlib import Path
here=Path(__file__).resolve().parent
class AcceptanceTests(unittest.TestCase):
 def check_case(self,status,outcome,state):
  with tempfile.TemporaryDirectory()as tmp:
   p=Path(tmp);tag=p/'single-123';tag.mkdir();(tag/'whole.console').write_text('RESULT pass acked=128 failed=0 rev=129 hash=123\n')
   v={'outcome':outcome,'host_seconds':1,'sim_seconds':2,'world_witness':'w','frames_delivered':0,'machines':[{'name':'whole','state':state,'witness':'w','state_digest':'d','branches':1,'exits':1}]}
   tag.with_suffix('.json').write_text(json.dumps(v));tag.with_suffix('.status').write_text(str(status));tag.with_suffix('.wall-ns.txt').write_text('1000000000');tag.with_suffix('.err').write_text('')
   (p/'budget.json').write_text(json.dumps({'wall_seconds':1,'limit_seconds':360}))
   result=subprocess.run([sys.executable,str(here/'analyze.py'),str(p)],capture_output=True,text=True,check=True)
   return json.loads(result.stdout)['runs'][0]['accepted']
 def test_nonzero_process_status_rejected(self):self.assertFalse(self.check_case(1,'end_machine_exited','Exited(0)'))
 def test_duration_outcome_rejected(self):self.assertFalse(self.check_case(0,'duration','Exited(0)'))
 def test_wrong_guest_exit_rejected(self):self.assertFalse(self.check_case(0,'end_machine_exited','Exited(1)'))
if __name__=='__main__':unittest.main()
