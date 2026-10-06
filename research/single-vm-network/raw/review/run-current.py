import hashlib,json,os,pathlib,subprocess,time
root=pathlib.Path('/home/gvickers/seedbox-single-vm-network');os.chdir(root)
results=root/'review-results';initial=json.loads((root/'results/budget.json').read_text())['wall_seconds'];used=initial
budget=results/'budget.json'
if budget.exists():raise SystemExit('refuse duplicate current validation')
healthy=json.loads((root/'research/single-vm-network/single.json').read_text())
healthy['machines'][0]['cmdline']=healthy['machines'][0]['cmdline'].replace('seedbox.concurrency=2 seedbox.writes=64','seedbox.concurrency=8 seedbox.writes=16')
(results/'healthy.json').write_text(json.dumps(healthy,indent=2)+'\n')
summary=[]
for name in ('controls','fault-a','fault-b','negative','healthy'):
 remaining=360-used
 if remaining<=0:raise SystemExit('360s aggregate budget exhausted')
 source='fault' if name.startswith('fault-') else name
 world=results/'healthy.json' if name=='healthy' else root/f'research/single-vm-network/{source}.json'
 (results/f'{name}.config.json').write_text(world.read_text())
 limit=min(120,remaining);t=time.monotonic();status=1
 try:
  with (results/f'{name}.batch.log').open('x') as log:
   status=subprocess.run(['bash','sim/batch.sh',str(world),'review-current/'+name,'1009','--digest'],env={**os.environ,'SEEDBOX_ROOT':str(root),'CPU0':'14','SEEDBOX_TIMEOUT':str(limit),'SEEDBOX_MARGIN':'16000'},stdout=log,stderr=subprocess.STDOUT).returncode
 finally:
  elapsed=time.monotonic()-t;used+=elapsed
  budget.write_text(json.dumps({'historical_wall_seconds':initial,'new_wall_seconds':used-initial,'wall_seconds':used,'limit_seconds':360},indent=2)+'\n')
 out=root/f'runs/review-current/{name}-s1009-r0'
 report=json.loads(pathlib.Path(str(out)+'.json').read_text()) if pathlib.Path(str(out)+'.json').stat().st_size else {}
 console=(out/'client.console').read_text(errors='replace') if (out/'client.console').exists() else ''
 attempt_status=pathlib.Path(str(out)+'.status').read_text().strip()
 if name=='controls':accepted=status==0 and attempt_status=='0' and report.get('outcome')=='end_machine_exited' and report.get('machines',[{}])[0].get('state')=='Exited(0)' and 'NETWORK controls-pass' in console and console.count('ONEWAY')==4 and all('pass=1' in line for line in console.splitlines() if line.startswith('ONEWAY'))
 else:
  parsed=subprocess.run(['python3','sim/campaign-result.py',str(out)+'.json','1009'],capture_output=True,text=True)
  marker='RESULT fail' if name=='negative' else 'RESULT pass'
  accepted=status==0 and parsed.returncode==0 and marker in parsed.stdout
  if name=='negative':accepted=accepted and 'missing 1 acknowledged writes' in console
  if name=='healthy':accepted=accepted and 'RESULT pass acked=128 failed=0' in console
 item={'name':name,'supervisor_status':status,'attempt_status':attempt_status,'accepted':accepted,'wall_seconds':elapsed,'host_seconds':report.get('host_seconds'),'guest_ns':report.get('virtual_ns'),'result':[line for line in console.splitlines() if line.startswith(('RESULT','SEEDBOX_PHASE'))],'world_witness':report.get('world_witness')}
 summary.append(item);(results/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');print(json.dumps(item),flush=True)
 if not accepted:raise SystemExit('acceptance failed; stopping bounded validation')
a=root/'runs/review-current/fault-a-s1009-r0';b=root/'runs/review-current/fault-b-s1009-r0';ra=json.loads(pathlib.Path(str(a)+'.json').read_text());rb=json.loads(pathlib.Path(str(b)+'.json').read_text())
comparisons={'world_witness_equal':ra['world_witness']==rb['world_witness'],'state_digest_equal':ra['machines'][0]['state_digest']==rb['machines'][0]['state_digest'],'machine_witness_equal':ra['machines'][0]['witness']==rb['machines'][0]['witness'],'console_equal':(a/'client.console').read_bytes()==(b/'client.console').read_bytes(),'mgmt_equal':(a/'client.mgmt').read_bytes()==(b/'client.mgmt').read_bytes()}
(results/'fault-replay.json').write_text(json.dumps(comparisons,indent=2)+'\n');print(json.dumps(comparisons),flush=True)
if not all(comparisons.values()):raise SystemExit('fault replay differed')
