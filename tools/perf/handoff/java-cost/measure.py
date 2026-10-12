import json, os, subprocess as sp, time, threading, hashlib
from pathlib import Path
O=Path(__file__).resolve().parent
cli=str(O/'seedbox'); env=dict(os.environ,SEEDBOX_STATE=str(O/'state'))
start=time.monotonic(); deadline=start+1800
commands=[]
def run(args,out,timeout=120):
 t=time.monotonic(); p=sp.run(args,env=env,stdout=sp.PIPE,stderr=sp.PIPE,timeout=timeout)
 (O/(out+'.stdout')).write_bytes(p.stdout); (O/(out+'.stderr')).write_bytes(p.stderr)
 commands.append({'argv':args,'rc':p.returncode,'wall':time.monotonic()-t}); (O/'commands.json').write_text(json.dumps(commands,indent=2))
 return p

def snap(name):
 for suffix,args in [('ps',['ps','-axo','pid,ppid,%cpu,etime,command']),('load',['uptime']),('containers',['podman','ps','--no-trunc']),('thermal',['pmset','-g','therm'])]:run(args,name+'-'+suffix,20)
serverlog=(O/'server.log').open('wb')
server=sp.Popen([cli,'serve','--state',str(O/'state'),'--full-state'],env=env,stdout=serverlog,stderr=serverlog)
try:
 # Wait only for our newly owned server startup, never old writer.
 for i in range(50):
  if server.poll() is not None: raise RuntimeError('server startup failed')
  p=run([cli,'get','capacity','--json'],'startup',10)
  if p.returncode==0:break
  time.sleep(.1)
 else:raise RuntimeError('server not ready')
 samples=[]
 for pair in range(5):
  for mode in (['off','on'] if pair%2==0 else ['on','off']):
   if time.monotonic()>deadline-300:raise RuntimeError('total cap')
   tag=f'{pair}-{mode}'; snap(tag+'-before')
   submitted=time.monotonic_ns()
   p=run([cli,'run',str(O/(mode+'.json')),'--detach','--json','--wall-time','180s'],tag+'-submit',210)
   if p.returncode:raise RuntimeError('submission failed '+tag)
   resource=json.loads(p.stdout); id=resource['metadata']['id']
   arrivals=[]
   logerr=(O/(tag+'-live.stderr')).open('wb')
   follower=sp.Popen([cli,'logs',id,'--follow','--json'],env=env,stdout=sp.PIPE,stderr=logerr)
   def readlogs():
    with (O/(tag+'-live.jsonl')).open('w') as f:
     for line in follower.stdout:
      r={'hostMonotonicNs':time.monotonic_ns(),'sinceSubmitNs':time.monotonic_ns()-submitted,'record':json.loads(line)}
      arrivals.append(r); f.write(json.dumps(r)+'\n'); f.flush()
   thread=threading.Thread(target=readlogs); thread.start()
   try:p=run([cli,'wait',id,'--for','completed','--timeout','200s','--json'],tag+'-wait',210)
   finally:
    try:follower.wait(timeout=10)
    except sp.TimeoutExpired:follower.terminate(); follower.wait(timeout=10)
    thread.join(); logerr.close()
   run([cli,'describe','simulation',id,'--json'],tag+'-result')
   r=json.loads((O/(tag+'-result.stdout')).read_bytes())
   for suffix,args in [('logs',['logs',id]),('events',['events',id]),('diagnostics',['diagnostics',id,'--json']),('vmm',['diagnostics',id,'vmm.json']),('world',['diagnostics',id,'world.json'])]:run([cli]+args,tag+'-'+suffix)
   exp=run([cli,'export',id,'-o',str(O/(tag+'.seedbox.tar'))],tag+'-export')
   if exp.returncode:raise RuntimeError('export failed; preserving simulation '+id)
   bundle=O/(tag+'.seedbox.tar')
   sha=hashlib.file_digest(bundle.open('rb'),'sha256').hexdigest()
   sample={'pair':pair,'mode':mode,'id':id,'status':r['status'],'bundleSha256':sha,'accepted':r['status'].get('verdict')=='passed' and r['status'].get('oracleExitCode')==0}
   samples.append(sample); (O/'samples.json').write_text(json.dumps(samples,indent=2))
   run([cli,'delete','simulation',id],tag+'-delete')
   snap(tag+'-after'); print(tag,id,sample['accepted'],r['status'].get('stageTimings'),flush=True)
finally:
 try:run([cli,'shutdown'],'shutdown',120)
 finally:
  try:server.wait(timeout=30)
  except sp.TimeoutExpired:server.terminate();server.wait(timeout=10)
  serverlog.close()
