import json,pathlib,subprocess,sys,time,os,signal
ROOT=pathlib.Path('/tmp/seedbox-round-1.S74xJ4');E=ROOT/'evidence/w5a';P=ROOT/'amd-counter'
plan=json.loads(pathlib.Path(sys.argv[1]).read_text());batch=next(b for b in plan['batches'] if b['id']==sys.argv[2]);attempts=[a for a in plan['attempts'] if a['batch']==batch['id']]
assert len(attempts)==22 and all(a['args'][0]=='linux' and a['args'][1] in ('100000','1000100') and a['args'][2]=='10000' and a['args'][3] in ('correct','negative','reference') for a in attempts)
def profile(stage):
 p=subprocess.run(['/etc/profiles/per-user/gvickers/bin/python3',str(P/'host_profile.py')],capture_output=True,text=True)
 (E/batch['host_'+stage]).write_text(p.stdout);assert p.returncode==0,p.stderr+p.stdout
def ledger(v):
 with (E/batch['ledger']).open('a') as f:f.write(json.dumps(v)+'\n');f.flush();os.fsync(f.fileno())
assert not (E/batch['ledger']).exists(),'batch ledger already exists; do not reuse attempted IDs'
profile('before');loads=[];failed=False
try:
 if batch['load']=='loaded':
  for cpu in [2,18,3,19]:loads.append(subprocess.Popen(['taskset','-c',str(cpu),'yes'],stdout=subprocess.DEVNULL,start_new_session=True))
 for a in attempts:
  ledger({'attempt_id':a['id'],'state':'started','time_ns':time.time_ns()})
  cmd=[str(ROOT/'run.sh'),'w5a','sudo','-n','/etc/profiles/per-user/gvickers/bin/python3',str(P/'msr_guard.py'),'--workaround','original','--receipt',str(E/a['receipt']),'--probe-sha256',plan['probe_sha256'],*a['args']]
  p=subprocess.run(cmd,text=True,capture_output=True);evidence=[l.removeprefix('Evidence: ') for l in p.stdout.splitlines() if l.startswith('Evidence: ')]
  ledger({'attempt_id':a['id'],'state':'finished','time_ns':time.time_ns(),'wrapper_exit':p.returncode,'evidence':evidence,'wrapper_stdout':p.stdout,'wrapper_stderr':p.stderr})
  print(json.dumps({'attempt_id':a['id'],'wrapper_exit':p.returncode,'evidence':evidence}),flush=True)
  if p.returncode!=0:failed=True
  if p.returncode==75:break
finally:
 for p in loads:
  if p.poll() is None:os.killpg(p.pid,signal.SIGTERM)
 for p in loads:
  try:p.wait(timeout=3)
  except subprocess.TimeoutExpired:os.killpg(p.pid,signal.SIGKILL);p.wait()
 profile('after')
raise SystemExit(1 if failed else 0)
