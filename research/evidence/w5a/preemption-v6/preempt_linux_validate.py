"""Validate the entire declared deciding matrix, including missing attempts."""
import argparse,collections,json,pathlib,sys
p=argparse.ArgumentParser();p.add_argument('evidence');p.add_argument('--plan',required=True);p.add_argument('--batch');a=p.parse_args();root=pathlib.Path(a.evidence);plan=json.loads(pathlib.Path(a.plan).read_text())
failures=[];status=[];records={};group_summary=[]
def need(ok,reason,where):
 if not ok:failures.append({'where':str(where),'reason':reason})
def read_json(path):
 try:return json.loads(path.read_text())
 except (OSError,ValueError) as e:need(False,'missing or invalid JSON: '+str(e),path);return None
def inventory(row):
 before=row.get('tasks_before',[]);after=row.get('tasks_after',[]);tid=str(row['single_thread_tid']);aux=[t for t in after if t['tid']!=tid]
 return len(before)==1 and before[0]['tid']==tid and sum(t['tid']==tid for t in after)==1 and len(aux)<=1 and all(t['comm'].strip()=='kvm-nx-lpage-re' and 'State:\tS (sleeping)' in t['status'] for t in aux)
need(plan['probe_sha256']=='1e01152e2ff5e40754361db6a2accae0e049477e91c3897641a21495787ab172','wrong reviewed probe',a.plan)
need(plan['guard_sha256']=='fed00db515067bc04a8995890c97270d599a2f394931c0c340dc1b3eededa559','wrong reviewed guard',a.plan)
all_attempts=plan['attempts'];need(len(all_attempts)==44 and len({x['id'] for x in all_attempts})==44 and len({x['receipt'] for x in all_attempts})==44,'plan must contain all 44 unique attempts',a.plan)
batches=[b for b in plan['batches'] if not a.batch or b['id']==a.batch];need(len(batches)==(1 if a.batch else 2),'missing declared batch',a.plan)
attempts=[x for x in all_attempts if x['batch'] in {b['id'] for b in batches}];selected={x['receipt']:x for x in attempts};known={x['receipt'] for x in all_attempts};ledgers={}
for b in batches:
 for stage in ('before','after'):
  v=read_json(root/b['host_'+stage])
  if v:need(v.get('matches_pinned_profile') is True and v.get('profile')==plan['pinned_host_profile'],'pinned host profile differs',b['host_'+stage])
 path=root/b['ledger'];lines=[]
 try:lines=[json.loads(l) for l in path.read_text().splitlines()]
 except (OSError,ValueError) as e:need(False,'missing or invalid batch ledger: '+str(e),path)
 for x in lines:ledgers.setdefault(x['attempt_id'],[]).append(x)
 expected={(str(t),policy):n for t in (100000,1000100) for policy,n in [('correct',5),('negative',5),('reference',1)]}
 actual=collections.Counter((x['args'][1],x['args'][3]) for x in attempts if x['batch']==b['id'])
 need(actual==expected,'incomplete declared target/reference/negative matrix',b['id'])
for d in root.glob('run.*'):
 if not d.is_dir() or not (d/'stderr.txt').exists():continue
 markers=[]
 try:markers=[json.loads(l) for l in (d/'stderr.txt').read_text().splitlines() if l.startswith('{"guard_receipt"')]
 except ValueError:need(False,'malformed guard marker',d);continue
 for mark in markers:
  name=pathlib.Path(mark['guard_receipt']).name
  if name not in selected:
   if name in known or name in plan.get('excluded_smoke_receipts',[]):continue
   rfile=root/name
   if rfile.exists():
    extra=read_json(rfile)
    if extra and extra.get('reviewed_probe_sha256')==plan['probe_sha256']:need(False,'unplanned measured receipt',name)
   continue
  need(name not in records,'duplicate raw record for receipt',name)
  receipt=read_json(root/name);row=None
  try:
   values=[json.loads(l) for l in (d/'stdout.txt').read_text().splitlines() if l.startswith('{"')];values=[v for v in values if v.get('mode')=='linux'];need(len(values)==1,'expected one Linux result row',d);row=values[0] if len(values)==1 else None
   exitcode=int((d/'exit-code.txt').read_text())
  except (OSError,ValueError) as e:need(False,'invalid or missing raw result: '+str(e),d);exitcode=None
  records[name]={'evidence':str(d),'receipt':receipt,'row':row,'exitcode':exitcode}
for spec in attempts:
 name=spec['receipt'];events=ledgers.get(spec['id'],[]);starts=[e for e in events if e['state']=='started'];ends=[e for e in events if e['state']=='finished'];q=records.get(name)
 state='not_executed' if not starts else 'attempted_without_valid_result' if not q or not q['receipt'] or not q['row'] else 'measured'
 status.append({'attempt_id':spec['id'],'state':state,'evidence':q['evidence'] if q else None})
 need(len(starts)==len(ends)==1,'missing/duplicate started or finished attempt ledger',spec['id'])
 need(state=='measured','declared attempt did not produce a receipt and result',spec['id'])
 if state!='measured':continue
 r=q['receipt'];v=q['row'];where=q['evidence']
 need(len(ends)==1 and ends[0].get('wrapper_exit')==0 and len(ends[0].get('evidence',[]))==1 and pathlib.Path(ends[0]['evidence'][0]).name==pathlib.Path(where).name,'wrapper/ledger evidence mismatch',where)
 need(r['guard_sha256']==plan['guard_sha256'] and r['reviewed_probe_sha256']==plan['probe_sha256'] and r['mode']=='original' and r['child_args']==spec['args'],'reviewed receipt/profile/arguments differ',where)
 need(r['cpus']==[2,18] and r['msr']=='0xc0011020' and r['bit']==54 and r['original']==r['restored']==plan['original_msrs'] and r['applied']=={} and not r['errors'] and not r.get('timed_out'),'MSR/profile/restore guard failure',where)
 need(q['exitcode']==r.get('child_exit')==spec['expected_exit'],'raw/guard exit mismatch',where)
 need(v['target']==int(spec['args'][1]) and v['margin']==10000 and bool(v.get('reference'))==(spec['kind']=='reference') and (spec['kind']=='reference' or v['correction']==(spec['kind']=='positive')),'result target or policy differs from plan',where)
 need(v['perf_config']==0x5100d1 and v['perf_flags']==524293 and inventory(v),'counter or owner/auxiliary-task profile differs',where)
 need(v['input_state']['manifest_sha256']==plan['input_manifest_sha256'] and all(v['input_state'][k].startswith('Ok(') for k in ['mp_state','debug_registers','vcpu_events']),'pinned Linux input or getter differs',where)
groups=collections.defaultdict(list)
for spec in attempts:
 q=records.get(spec['receipt'])
 if q and q['row']:groups[(spec['batch'],int(spec['args'][1]),spec['kind'])].append(q)
for b in batches:
 for target in (100000,1000100):
  refs=groups[(b['id'],target,'reference')];pos=groups[(b['id'],target,'positive')];neg=groups[(b['id'],target,'negative')]
  need(len(refs)==1 and len(pos)==len(neg)==5,'incomplete measured matrix',b['id']+':'+str(target))
  for kind,rs in [('positive',pos),('negative',neg)]:
   need(len({q['row']['single_thread_tid'] for q in rs})==5,'requires five distinct fresh processes',b['id']+':'+kind+':'+str(target))
   init={(json.dumps(q['row']['initial_registers'],sort_keys=True),q['row']['initial_ram_sha256'],json.dumps(q['row']['input_state'],sort_keys=True)) for q in rs+refs}
   need(len(init)==1,'initial CPU/RAM/input witness differs',b['id']+':'+kind+':'+str(target))
   ids={(q['row']['final_count'],q['row']['final_registers']['rip'],q['row']['registers_sha256'],q['row']['ram_sha256'],q['row']['debug_registers'],q['row']['vcpu_events']) for q in rs}
   need(len(ids)==1 if kind=='positive' else len(ids)>1,'landing identity equality/variation requirement failed',b['id']+':'+kind+':'+str(target))
   group_summary.append({'batch':b['id'],'target':target,'kind':kind,'measured':len(rs),'initial_variants':len(init),'landing_variants':len(ids),'fresh_tids':len({q['row']['single_thread_tid'] for q in rs})})
  for q in pos:
   v=q['row'];r=v['final_registers'];progress=target%1000000;left=1000000-progress;flags=0x202|(4 if (left&255).bit_count()%2==0 else 0)|(16 if left&15==15 else 0)
   expected=0x401097 if target==100000 else 0x4010b2
   need(v['accepted'] and v['landed'] and not v['overshoot'] and v['analytic_valid'] and v['perf_valid'] and v['final_count']==target and r['rip']==expected and r['rax']==progress and r['rcx']==left and r['rflags']==flags,'incorrect analytic landing state',q['evidence'])
   need(v['signals']>=1 and v['watchdog_alarms']==0 and v['time_enabled_ns']>0 and v['time_enabled_ns']==v['time_running_ns'] and v['host_admin_count_before']==v['host_admin_count_after']==0,'invalid signal/counter interval or host exclusion',q['evidence'])
   need(v['debug_registers'].startswith('Ok(') and v['vcpu_events'].startswith('Ok('),'failed landing state getters',q['evidence'])
   if target==1000100:
    need(any(x['tag']==3 and x['phase']=='correction' and x['count']==1000000 and x['rip']==0x4010a8 and x['completion']['before_count']==x['completion']['after_count'] and x['completion']['after_registers']['rip']==0x4010a8 for x in v['pio_boundaries']),'intervening correction PIO completion missing or invalid',q['evidence'])
   t=v.get('terminal');need(t is not None and v['terminal_error'] is None,'failed continuation after debug teardown',q['evidence'])
   if t is not None:
    fields=('count','registers_sha256','ram_sha256','debug_registers','vcpu_events')
    need(len(refs)==1 and all(t[f]==refs[0]['row']['terminal'][f] for f in fields),'terminal differs from fresh uninterrupted reference',q['evidence'])
    need(t['count']==2000000 and t['registers']['rip']==0x4010c3 and t['registers']['rax']==2 and t['registers']['rcx']==0 and t['registers']['rdi']==t['registers']['rsi']==1000000 and t['time_enabled_ns']>0 and t['time_enabled_ns']==t['time_running_ns'] and t['debug_registers'].startswith('Ok(') and t['vcpu_events'].startswith('Ok('),'terminal analytic state/interval invalid',q['evidence'])
  for q in refs:
   t=q['row'].get('terminal');need(t is not None and t['time_enabled_ns']>0 and t['time_enabled_ns']==t['time_running_ns'] and t['debug_registers'].startswith('Ok(') and t['vcpu_events'].startswith('Ok('),'invalid uninterrupted reference',q['evidence'])
print(json.dumps({'scope':'partial declared batch' if a.batch else 'entire declared 44-attempt matrix','accepted':not failures,'planned':len(attempts),'measured':sum(s['state']=='measured' for s in status),'attempt_status':status,'groups':group_summary,'failures':failures},indent=2));raise SystemExit(1 if failures else 0)
