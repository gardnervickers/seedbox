import hashlib, json,tarfile
from pathlib import Path
import freeze
O=Path(__file__).resolve().parent; E=freeze.E
H=lambda b:hashlib.sha256(b).hexdigest()
spec=json.loads((O/'off.json').read_text())['spec']; on=json.loads((O/'on.json').read_text())['spec']
# Only boot archives differ in structured inputs.
a=json.loads(json.dumps(spec)); b=json.loads(json.dumps(on))
for m in [a,b]:
 m['machines']['java'].pop('bootArchive');m['oracle'].pop('bootArchive')
assert a==b
runtime={}; launchers={}
for mode,n in [('off','baseline-runtime.tar.gz'),('on','runtime.tar.gz')]:
 with tarfile.open(E/n) as t:
  runtime[mode]={m.name:H(t.extractfile(m).read()) for m in t.getmembers()}
  launchers[mode]=t.extractfile('compose-init').read()
assert {k:v for k,v in runtime['off'].items() if k!='compose-init'}=={k:v for k,v in runtime['on'].items() if k!='compose-init'}
with tarfile.open(E/'runtime.tar.gz') as t:
 import io
 with tarfile.open(fileobj=io.BytesIO(t.extractfile('progress.tar').read())) as p:
  progress={m.name:H(p.extractfile(m).read()) for m in p.getmembers() if m.isfile()}
boot={}
for name,offm,onm in [('java',spec['machines']['java'],on['machines']['java']),('oracle',spec['oracle'],on['oracle'])]:
 blobs={mode:(O/'state/cache/sha256'/m['bootArchive'].split(':')[1]).read_bytes() for mode,m in [('off',offm),('on',onm)]}
 tables={mode:{path:(size,H(data)) for path,start,size,data in freeze.entries(blob)} for mode,blob in blobs.items()}
 delta=[path for path in tables['off'] if tables['off'][path]!=tables['on'][path]];assert delta==['init'],delta
 records={}
 for path,start,size,data in freeze.entries(blobs['off']):
  if path.endswith(('libdynamorio.so','libseedbox_progress.so','libdrreg.so','libdrmgr.so','Workload.class','Workload.java','oracle.py','drrun')):records[path]={'size':size,'sha256':H(data)}
 boot[name]={'changedEntries':delta,'payloads':records}
print(json.dumps(boot,indent=2))
(O/'audit.json').write_text(json.dumps({'runtime':runtime,'progress':progress,'boots':boot},indent=2))
artifacts={str(p.relative_to(O)):{'size':p.stat().st_size,'sha256':hashlib.file_digest(p.open('rb'),'sha256').hexdigest()} for p in O.rglob('*') if p.is_file() and not str(p.relative_to(O)).startswith('state/') and p.name!='artifact-sha256.json'}
(O/'artifact-sha256.json').write_text(json.dumps(artifacts,indent=2))
