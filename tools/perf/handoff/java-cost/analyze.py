import json, statistics, tarfile,hashlib
from pathlib import Path
O=Path(__file__).resolve().parent
samples=json.loads((O/'samples.json').read_text()); rows=[]
for s in samples:
 tag=f"{s['pair']}-{s['mode']}"; r=json.loads((O/(tag+'-result.stdout')).read_text()); v=json.loads((O/(tag+'-vmm.stdout')).read_text())
 lines=[json.loads(line) for line in (O/(tag+'-live.jsonl')).read_text().splitlines()]
 receipts=[]
 for line in lines:
  record=line['record']
  if record['type']=='entry' and record['entry']['type']=='bytes':
   e=record['entry']; receipts.append((bytes(e['bytes']).decode(),line['hostMonotonicNs']))
 def at(prefix):
  found=[t for text,t in receipts if text.startswith(prefix)]; assert len(found)==1,(tag,prefix);return found[0]
 phases={
 'oracle_wait_to_jfr_running':(at('java-jit: JFR RUNNING')-at('java oracle: waiting'))/1e9,
 'jfr_running_to_final_checksum':(at('java-jit: seed=123')-at('java-jit: JFR RUNNING'))/1e9,
 'final_checksum_to_jfr_stopped':(at('java-jit: JFR STOPPED')-at('java-jit: seed=123'))/1e9,
 'jfr_stopped_to_java_completed':(at('java-jit: completed')-at('java-jit: JFR STOPPED'))/1e9,
 'java_completed_to_oracle_pass':(at('java oracle: three checksums')-at('java-jit: completed'))/1e9}
 logs=(O/(tag+'-logs.stdout')).read_text()
 for text in ['checksum=17286622441068077348','checksum=868493419600900836','checksum=1222772367379087776','JFR RUNNING compiler=HotSpot 64-Bit Tiered Compilers','JFR STOPPED bytes=','java-jit: completed','java oracle: three checksums and real JFR dump passed']:assert text in logs
 assert s['accepted']; assert r['spec']==json.loads((O/(s['mode']+'.json')).read_text())['spec']
 # Byte proof: bundles retain the exact specified boot archives and engine.
 with tarfile.open(O/(tag+'.seedbox.tar')) as t:
  for digest in [r['spec']['runtime']['kernel'],r['spec']['runtime']['engine'],r['spec']['machines']['java']['bootArchive'],r['spec']['oracle']['bootArchive']]:
   h=digest.split(':')[1]; assert hashlib.file_digest(t.extractfile('objects/'+h),'sha256').hexdigest()==h
 stage={k:x['elapsedWallNs']/1e9 for k,x in r['status']['stageTimings'].items()}
 counts={m['name']:{'exits':m['application_evidence']['exits'],'simulatedTimeNs':m['application_evidence']['time_ns']} for m in v['machines']}
 rows.append({'pair':s['pair'],'mode':s['mode'],'id':s['id'],'stage':stage,'receiptPhases':phases,'scheduler':v['scheduler']['host'],'counts':counts,'simulatedTimeNs':r['status']['simulatedTimeNs']})
summary={}
for mode in ['off','on']:
 selected=[r for r in rows if r['mode']==mode]
 summary[mode]={category:{k:{'median':statistics.median(r[category][k] for r in selected),'min':min(r[category][k] for r in selected),'max':max(r[category][k] for r in selected)} for k in selected[0][category]} for category in ['stage','receiptPhases','scheduler']}
print(json.dumps(summary,indent=2)); (O/'analysis.json').write_text(json.dumps({'rows':rows,'summary':summary},indent=2))
