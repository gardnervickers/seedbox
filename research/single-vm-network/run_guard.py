#!/usr/bin/env python3
"""Root supervisor: owns child group, bounded attempts, private MSR cleanup."""
import fcntl,json,os,signal,subprocess,sys,time
from pathlib import Path
def stop_owned_group(child):
 if child is None or child.poll() is not None:return
 try:os.killpg(child.pid,signal.SIGTERM)
 except ProcessLookupError:child.wait();return
 try:child.wait(timeout=2)
 except subprocess.TimeoutExpired:
  try:os.killpg(child.pid,signal.SIGKILL)
  except ProcessLookupError:pass
  child.wait()

def main():
 root=Path(__file__).resolve().parents[2];os.chdir(root)
 if not sys.argv[1:]:raise SystemExit('pass world names: controls controls multi single multi single negative fault')
 assert os.geteuid()==0,'run via sudo'
 os.sched_setaffinity(0,{14})
 results=root/os.environ.get('RESULTS_DIR','results');results.mkdir(exist_ok=True)
 lockpath='/tmp/seedbox-perf-cpu-14.lock'
 try:lock=open(lockpath,'r')
 except FileNotFoundError:lock=os.fdopen(os.open(lockpath,os.O_RDONLY|os.O_CREAT|os.O_EXCL,0o644),'r')
 fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
 snapshot=results/f'msr-snapshot-14,30-{os.getpid()}-{time.time_ns()}.json'
 assert not snapshot.exists()
 msr=root/'research/single-vm-network/msr.py';log=open(results/'msr.log','a')
 budgetfile=results/'budget.json';used=json.loads(budgetfile.read_text())['wall_seconds']if budgetfile.exists()else 0.
 child=None
 class Interrupted(Exception):pass
 def on_signal(sig,frame):raise Interrupted(sig)
 for s in (signal.SIGTERM,signal.SIGINT,signal.SIGHUP):signal.signal(s,on_signal)
 def reap():
  nonlocal child
  stop_owned_group(child)
  child=None
 status=0
 try:
  subprocess.run([sys.executable,str(msr),'set',str(snapshot),'14,30'],stdout=log,check=True)
  for name in sys.argv[1:]:
   remaining=360-used
   if remaining<=0:raise RuntimeError('360-second aggregate attempt budget exhausted')
   limit=min(120,remaining);tag=f'{name}-{time.time_ns()}';out=results/tag
   command=['taskset','-c','14',str(root/'sim/vmm/target/release/seedbox-vmm'),'--config',str(root/f'research/single-vm-network/{name}.json'),'--seed',os.environ.get('SEED','1009'),'--out',str(out),'--digest']
   Path(str(out)+'.inputs.txt').write_text((results/'input-sha256.txt').read_text())
   Path(str(out)+'.config.json').write_text((root/f'research/single-vm-network/{name}.json').read_text())
   t=time.monotonic();attempt_status=1
   try:
    with open(str(out)+'.json','w')as stdout,open(str(out)+'.err','w')as stderr:
     child=subprocess.Popen(command,stdout=stdout,stderr=stderr,start_new_session=True,env={**os.environ,'SEEDBOX_MARGIN':'16000'})
     try:attempt_status=child.wait(timeout=limit)
     except subprocess.TimeoutExpired:attempt_status=124;reap()
    if attempt_status==0:
     console=''.join(f.read_text(errors='replace')for f in out.glob('*.console'))
     if name=='controls':accepted='NETWORK controls-pass' in console and 'INIT exit=0' in console
     elif name=='negative':accepted='RESULT fail' in console and 'missing 1 acknowledged writes' in console
     elif name=='resources':accepted=any('DIAG n1'in f.read_text()for f in out.glob('*.mgmt'))
     elif name=='debug':accepted='RESULT pass'in console or 'RESULT fail'in console
     else:accepted='RESULT pass' in console
     if not accepted:
      attempt_status=65
      with open(str(out)+'.err','a')as err:err.write('guest acceptance marker missing\n')
   except Interrupted as e:
    attempt_status=128+e.args[0];raise
   finally:
    try:reap()
    finally:
     wall=time.monotonic()-t;used+=wall
     Path(str(out)+'.wall-ns.txt').write_text(str(round(wall*1e9))+'\n')
     Path(str(out)+'.status').write_text(str(attempt_status)+'\n')
     budgetfile.write_text(json.dumps({'wall_seconds':used,'limit_seconds':360})+'\n')
   print(f'{tag} status={attempt_status} wall={wall:.3f} used={used:.3f}',flush=True)
   if attempt_status:status=attempt_status;break
 except Interrupted as e:status=128+e.args[0]
 except Exception as e:print(f'guard: {e}',file=sys.stderr);status=1
 finally:
  for s in (signal.SIGTERM,signal.SIGINT,signal.SIGHUP):signal.signal(s,signal.SIG_IGN)
  try:
   reap()
  finally:
   if snapshot.exists():
    shown=subprocess.run([sys.executable,str(msr),'show',str(snapshot),'14,30'],stdout=log)
    restored=subprocess.run([sys.executable,str(msr),'restore',str(snapshot),'14,30'],stdout=log)
    if shown.returncode or restored.returncode:status=1
  log.close()
 return status

if __name__=="__main__":sys.exit(main())
