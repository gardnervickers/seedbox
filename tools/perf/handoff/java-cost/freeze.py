import copy, hashlib, json, tarfile
from pathlib import Path
E=Path('/Users/gardnervickers/.codex/worktrees/dr-jit-e2e/build/dr-guest-e2e')
O=Path(__file__).parent
H=lambda b: hashlib.sha256(b).hexdigest()
cache=O/'state/cache/sha256'; cache.mkdir(parents=True,exist_ok=True)
with tarfile.open(E/'baseline-pass/source.seedbox.tar') as t:
 manifest=json.load(t.extractfile('simulation.json'))
 spec=manifest['snapshot']['simulation']['spec']
 for m in t.getmembers():
  if m.name.startswith('objects/'):
   b=t.extractfile(m).read(); assert H(b)==m.name.split('/')[1]; (cache/H(b)).write_bytes(b)
with tarfile.open(E/'runtime.tar.gz') as t: on=t.extractfile('compose-init').read()
with tarfile.open(E/'baseline-runtime.tar.gz') as t: off=t.extractfile('compose-init').read()
def entries(b):
 p=0
 while p<len(b):
  assert b[p:p+6]==b'070701'
  size=int(b[p+54:p+62],16); ns=int(b[p+94:p+102],16)
  name=b[p+110:p+110+ns-1].decode(); start=(p+110+ns+3)&~3
  yield name,start,size,b[start:start+size]
  p=(start+size+3)&~3
  if name=='TRAILER!!!': break
identities={}
for mode in ['off','on']:
 s=copy.deepcopy(spec)
 for name,m in [('java',s['machines']['java']),('oracle',s['oracle'])]:
  old=m['bootArchive'].split(':')[1]; b=(cache/old).read_bytes()
  found=[]
  for path,start,size,data in entries(b):
   if path.endswith('compose-init') or path in ['init','./init']:
    if data==off: found.append((path,start,size))
   if path.endswith('oracle.py') or path.endswith('Workload.java'):
    (O/(name+'-'+Path(path).name)).write_bytes(data)
  assert len(found)==1,found
  path,start,size=found[0]; assert size==len(on)==len(off)
  if mode=='on': b=b[:start]+on+b[start+size:]
  digest=H(b); (cache/digest).write_bytes(b); m['bootArchive']='sha256:'+digest
  identities[mode+'-'+name]={'digest':digest,'replacedEntry':path,'size':size}
 (O/(mode+'.json')).write_text(json.dumps({'apiVersion':'seedbox/v1','kind':'Simulation','metadata':{'name':'java-cost-'+mode},'spec':s},indent=2))
identities['launcherOff']=H(off); identities['launcherOn']=H(on)
(O/'freeze.json').write_text(json.dumps(identities,indent=2))
print(json.dumps(identities,indent=2))
