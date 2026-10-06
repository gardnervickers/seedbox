#!/usr/bin/env python3
"""Historical 010d0e6 raw acceptance only; current fixtures use sim/campaign-result.py."""
import argparse,hashlib,json,re,statistics
from pathlib import Path
ap=argparse.ArgumentParser();ap.add_argument('results',type=Path);a=ap.parse_args()
rows=[];groups={}
for f in sorted(a.results.glob('*.json')):
 if not re.match(r'^(controls|multi|single|negative|fault|debug|resources)-\d+\.json$',f.name):continue
 label=f.name.split('-')[0];tag=f.with_suffix('');consoles=sorted(tag.glob('*.console'));console=''.join(c.read_text(errors='replace')for c in consoles)
 configfile=f.with_suffix('.config.json')
 config=json.loads(configfile.read_text())if configfile.exists()else{'mem_mb':512}
 identity=hashlib.sha256(f.with_suffix('.inputs.txt').read_bytes()+configfile.read_bytes()).hexdigest()if f.with_suffix('.inputs.txt').exists()and configfile.exists()else 'historical-unrecorded'
 row={'mem_mb':config['mem_mb'],'input_identity':identity,'file':f.name,'variant':label,'status':int(f.with_suffix('.status').read_text()),'wall_seconds':int(f.with_suffix('.wall-ns.txt').read_text())/1e9,'accepted':False}
 try:v=json.loads(f.read_text())
 except json.JSONDecodeError:row['error']=f.with_suffix('.err').read_text();rows.append(row);continue
 expected_name='client'if label=='multi'else 'whole'
 expected_exit=1 if label=='negative'else 0
 terminated=row['status']==0 and v['outcome']=='end_machine_exited'and any(m['name']==expected_name and m['state']==f'Exited({expected_exit})'for m in v['machines'])
 row.update({'outcome':v['outcome'],'states':{m['name']:m['state']for m in v['machines']},'host_seconds':v['host_seconds'],'sim_seconds':v['sim_seconds'],'world_witness':v['world_witness'],'machine_witnesses':[m['witness']for m in v['machines']],'state_digests':[m['state_digest']for m in v['machines']],'console_sha256':[hashlib.sha256(c.read_bytes()).hexdigest()for c in consoles],'branches':sum(m['branches']for m in v['machines']),'exits':sum(m['exits']for m in v['machines']),'frames':v['frames_delivered'],'results':re.findall(r'^RESULT.*$',console,re.M),'phase':re.findall(r'^SEEDBOX_PHASE.*$',console,re.M),'load':re.findall(r'^LOAD.*$',console,re.M)})
 if terminated and label=='controls' and 'NETWORK controls-pass' in console:
  zero=console.split('NETWORK zero-delay')[1].split('NETWORK qdisc')[0]
  delayed=console.split('NETWORK fixed-delay')[1].split('NETWORK partition')[0]
  base=[int(x)for x in re.findall(r'rtt_ns=(\d+)',zero)];delay=[int(x)for x in re.findall(r'rtt_ns=(\d+)',delayed)]
  assert len(base)==len(delay)==3
  assert statistics.median(delay)-statistics.median(base)>=1500000,'delay increase below documented 1.5ms floor'
  assert len(set(re.findall(r'ISOLATION n\d net:\[(\d+)\]',console)))>=3
  assert 'errno=99' in console and console.count('expected=blocked')==2
  assert all('NETWORK qdisc host'+str(i) in console for i in range(1,5))
  assert 'pass=0'not in console and 'INIT exit=0'in console
  row['accepted']=True;row['zero_rtt_ns']=base;row['delayed_rtt_ns']=delay
 elif terminated and label in ('single','multi'):row['accepted']=bool(re.search(r'RESULT pass acked=128 failed=0 ',console))
 elif terminated and label=='negative':row['accepted']='RESULT fail' in console and 'missing 1 acknowledged writes'in console and 'RESULT incomplete'not in console
 elif terminated and label=='fault':
  mgmt=''.join(m.read_text()for m in tag.glob('*.mgmt'))
  row['accepted']='RESULT pass'in console and all(x in mgmt for x in ('n1 partitioned','n1 healed','n2 stopped','n2 started'))
  row['management']=mgmt
 if row['accepted']:groups.setdefault((label,config['mem_mb'],identity),[]).append(row)
 else:row['error']=f.with_suffix('.err').read_text()+'\n'+console
 rows.append(row)
summary={'runs':rows,'repeatability':{},'medians':{},'budget':json.loads((a.results/'budget.json').read_text())}
for (label,mem,identity),group in groups.items():
 key=f'{label}-{mem}-{identity[:12]}'
 if label in ('single','multi')and mem==1024:
  if label in summary['medians']:raise ValueError(f'mixed accepted input identities for comparison: {label}')
  summary['medians'][label]={k:statistics.median(r[k]for r in group)for k in ('wall_seconds','host_seconds','branches','exits')}
 checked=len(group)>1
 repeat={'runs':len(group),'tested':checked}
 for k in ('world_witness','machine_witnesses','state_digests','console_sha256'):
  repeat[k+'_equal']=len({json.dumps(r[k])for r in group})==1 if checked else None
  if checked:assert repeat[k+'_equal'],f'repeatability mismatch {label} {k}'
 summary['repeatability'][key]=repeat
if all(x in summary['medians']for x in ('multi','single')):summary['wall_ratio']=summary['medians']['multi']['wall_seconds']/summary['medians']['single']['wall_seconds']
print(json.dumps(summary,indent=2))
