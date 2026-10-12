import json,os,subprocess as sp,time,hashlib
from pathlib import Path
O=Path(__file__).resolve().parent; cli=str(O/'seedbox'); env=dict(os.environ,SEEDBOX_STATE=str(O/'state'))
commands=[]
def run(args,name,timeout=120):
 t=time.monotonic(); p=sp.run([cli]+args,env=env,stdout=sp.PIPE,stderr=sp.PIPE,timeout=timeout)
 (O/(name+'.stdout')).write_bytes(p.stdout); (O/(name+'.stderr')).write_bytes(p.stderr)
 commands.append({'argv':[cli]+args,'rc':p.returncode,'wall':time.monotonic()-t}); (O/'replay-commands.json').write_text(json.dumps(commands,indent=2)); return p
log=(O/'replay-server.log').open('wb'); server=sp.Popen([cli,'serve','--state',str(O/'state'),'--full-state'],env=env,stdout=log,stderr=log)
try:
 for i in range(50):
  if server.poll() is not None:raise RuntimeError('server exited')
  if run(['get','capacity','--json'],'replay-startup',10).returncode==0:break
  time.sleep(.1)
 else:raise RuntimeError('startup timeout')
 results=[]
 for mode in ['off','on']:
  tag='replay-'+mode
  p=run(['import',str(O/('0-'+mode+'.seedbox.tar')),'--json'],tag+'-import')
  if p.returncode:raise RuntimeError('import failed')
  source=json.loads(p.stdout)['metadata']['id']
  p=run(['replay',source,'--json','--wall-time','180s'],tag+'-result',210)
  report=json.loads(p.stdout); (O/(tag+'-parsed.json')).write_text(json.dumps(report,indent=2))
  print(mode,p.returncode,str(report)[:350],flush=True)
  # Replay comparison document contains rerun resource under simulation.
  sim=report.get('simulation',report.get('rerun',{}))
  if not isinstance(sim,dict) or 'metadata' not in sim:
   ids=json.loads(run(['get','simulations','--json'],tag+'-catalogue').stdout)
   (O/(tag+'-ids.json')).write_text(json.dumps(ids,indent=2))
   sims=ids if isinstance(ids,list) else ids.get('items',[])
   candidates=[r for r in sims if r['metadata']['id']!=source]
   assert len(candidates)==1,candidates
   sim=candidates[0]
  id=sim['metadata']['id']
  run(['describe','simulation',id,'--json'],tag+'-simulation')
  for suffix,args in [('logs',['logs',id]),('events',['events',id]),('diagnostics',['diagnostics',id,'--json']),('vmm',['diagnostics',id,'vmm.json']),('world',['diagnostics',id,'world.json'])]:run(args,tag+'-'+suffix)
  pexp=run(['export',id,'-o',str(O/(tag+'.seedbox.tar'))],tag+'-export')
  if pexp.returncode:raise RuntimeError('export failed')
  results.append({'mode':mode,'source':source,'replay':id,'rc':p.returncode,'sha256':hashlib.file_digest((O/(tag+'.seedbox.tar')).open('rb'),'sha256').hexdigest()})
  (O/'replays.json').write_text(json.dumps(results,indent=2))
  run(['delete','simulation',id],tag+'-delete'); run(['delete','simulation',source],tag+'-source-delete')
finally:
 try:run(['shutdown'],'replay-shutdown')
 finally:
  try:server.wait(timeout=30)
  except sp.TimeoutExpired:server.terminate();server.wait(timeout=10)
  log.close()
