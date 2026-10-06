"""Unprivileged durable recorder for reviewed original-profile cost attempts."""
import hashlib,json,os,pathlib,subprocess,sys,time
ROOT=pathlib.Path('/home/gvickers/seedbox-research/w5a-round-1')
E=ROOT/'evidence/w5a'
PY='/etc/profiles/per-user/gvickers/bin/python3'
SOURCE=ROOT/'amd-counter'
plan=json.loads(pathlib.Path(sys.argv[1]).read_text())
out=E/plan['batch_id'];out.mkdir()
(out/'expected-matrix.json').write_text(json.dumps(plan,indent=2)+'\n')
def profile(label):
 p=subprocess.run([PY,str(SOURCE/'host_profile.py')],capture_output=True,text=True)
 (out/(label+'.json')).write_text(p.stdout)
 (out/(label+'.stderr.txt')).write_text(p.stderr)
 if p.returncode:raise RuntimeError('host profile drift '+label)
def ledger(v):
 with (out/'ledger.jsonl').open('a') as f:
  f.write(json.dumps(v)+'\n');f.flush();os.fsync(f.fileno())
for path,sha in plan['identities'].items():
 if hashlib.sha256((SOURCE/path).read_bytes()).hexdigest()!=sha:raise RuntimeError('identity mismatch '+path)
profile('host-before')
try:
 for item in plan['attempts']:
  d=out/item['id'];d.mkdir()
  receipt=E/(item['id']+'.json')
  args=['sudo','-n',PY,str(SOURCE/'msr_guard.py'),'--workaround','original','--timeout','30','--receipt',str(receipt),'--probe-sha256',plan['identities']['target/release/preempt'],*map(str,item['args'])]
  (d/'command.json').write_text(json.dumps(args,indent=2)+'\n')
  (d/'cwd.txt').write_text(str(ROOT)+'\n')
  ledger({'id':item['id'],'state':'started','ns':time.time_ns()})
  with (d/'stdout.txt').open('w') as stdout,(d/'stderr.txt').open('w') as stderr:
   p=subprocess.run(args,cwd=ROOT,stdout=stdout,stderr=stderr)
  (d/'exit-code.txt').write_text(str(p.returncode)+'\n')
  ledger({'id':item['id'],'state':'finished','exit_code':p.returncode,'ns':time.time_ns()})
finally:profile('host-after')
print(json.dumps({'batch':str(out),'planned':len(plan['attempts'])}))
