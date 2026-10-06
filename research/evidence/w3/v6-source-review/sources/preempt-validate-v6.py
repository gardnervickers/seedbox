"""Strict acceptance, retaining all attempted records and guard failures."""
import argparse,collections,hashlib,json,pathlib,sys
p=argparse.ArgumentParser();p.add_argument('evidence');p.add_argument('--probe',required=True);p.add_argument('--guest',required=True);a=p.parse_args();root=pathlib.Path(a.evidence)
def owner_inventory(r):
 before=r.get('tasks_before',[]);after=r.get('tasks_after',[]);tid=str(r['single_thread_tid'])
 if len(before)!=1 or before[0]['tid']!=tid:return False
 owners=[t for t in after if t['tid']==tid];aux=[t for t in after if t['tid']!=tid]
 return len(owners)==1 and len(aux)<=1 and all(t['comm'].strip()=='kvm-nx-lpage-re' and 'State:\tS (sleeping)' in t['status'] for t in aux)
records=[];failures=[];groups=collections.defaultdict(list);refs=collections.defaultdict(list)
for d in root.glob('run.*'):
 if not (d/'stderr.txt').exists():continue
 receipt=None
 for line in (d/'stderr.txt').read_text().splitlines():
  if line.startswith('{"guard_receipt"'):
   f=root/pathlib.Path(json.loads(line)['guard_receipt']).name
   if f.exists():receipt=json.loads(f.read_text());receipt['_path']=str(f)
 if not receipt or receipt['reviewed_probe_sha256']!=a.probe:continue
 exitcode=int((d/'exit-code.txt').read_text())
 obj=None
 for line in (d/'stdout.txt').read_text().splitlines():
  if line.startswith('{"'):
   candidate=json.loads(line)
   if 'mode' in candidate:obj=candidate
 profile=(a.probe,a.guest,receipt.get('guard_sha256','MISSING_HELPER_HASH'),receipt['mode'],'loaded' if '-loaded-' in receipt['_path'] else 'idle')
 if obj: profile+= (obj['perf_config'],obj['perf_flags'])
 rec={'evidence':str(d),'receipt':receipt['_path'],'exitcode':exitcode,'guard_exit':receipt.get('child_exit'),'guard_errors':receipt['errors'],'row':obj};records.append(rec)
 guard_ok=receipt.get('guard_sha256')=='fed00db515067bc04a8995890c97270d599a2f394931c0c340dc1b3eededa559' and not receipt['errors'] and receipt['original']==receipt['restored'] and not receipt.get('timed_out')
 if receipt['mode']=='enabled':guard_ok &= all(int(receipt['applied'][str(c)],16)==int(receipt['original'][str(c)],16)|(1<<54) for c in (2,18))
 if not guard_ok:failures.append({'evidence':str(d),'reason':'guard/restore failure'})
 if obj is None:failures.append({'evidence':str(d),'reason':'no result row','stderr':(d/'stderr.txt').read_text()});continue
 key=profile+(obj['mode'],obj['target'],obj['margin'])
 if obj.get('reference'):refs[key].append(rec)
 else:groups[key+(obj['correction'],)].append(rec)
summary=[]
for key,rs in groups.items():
 corrected=key[-1];mode=key[-4];target=key[-3];margin=key[-2];rows=[r['row'] for r in rs]
 tids={r['single_thread_tid'] for r in rows}
 identities={(r['final_count'],r['final_registers']['rip'],r['registers_sha256'],r['ram_sha256'],r.get('debug_registers'),r.get('vcpu_events'),json.dumps(r.get('input_state'),sort_keys=True)) for r in rows}
 initials={(json.dumps(r['initial_registers'],sort_keys=True),r['initial_ram_sha256']) for r in rows}
 accepted=False;issues=[]
 if mode=='uncond':
  accepted=all(r['final_count']==0 and r['signals']==0 and r['watchdog_alarms']==1 and not r['landed'] for r in rows)
  kind='unsupported_branchfree_control'
 elif corrected and margin<100:
  accepted=all(r['overshoot'] and not r['accepted'] and q['exitcode']==2 and q['guard_exit']==2 for r,q in zip(rows,rs))
  kind='detected_overshoot_control'
 elif not corrected:
  accepted=len(rs)>=5 and len(tids)>=5 and len(initials)==1 and len(identities)>1 and all(q['exitcode']==0 and q['guard_exit']==0 for q in rs)
  kind='correction_off_variation_control'
 else:
  kind='positive'
  if len(rs)<5 or len(tids)<5:issues.append('requires at least five distinct fresh processes')
  if len(initials)!=1 or len(identities)!=1:issues.append('initial or landing states differ')
  for q,r in zip(rs,rows):
   reg=r['final_registers'];loops=target if mode=='rep' else target*2
   conditions=[q['exitcode']==0,q['guard_exit']==0,not q['guard_errors'],r['accepted'],r['landed'],not r['overshoot'],r['final_count']==target,reg['rip']==r['expected_rip'],r['analytic_valid'],r['perf_valid'],r['time_enabled_ns']>0,r['time_enabled_ns']==r['time_running_ns'],owner_inventory(r),r['signals']>=1,r['watchdog_alarms']==0,r['host_admin_count_before']==r['host_admin_count_after']==0,r['perf_config']==0x5100d1,r['perf_flags']==(1|4|(1<<19)),r['terminal'] is not None,r['terminal_error'] is None,r['debug_registers'].startswith('Ok('),r['vcpu_events'].startswith('Ok(')]
   if mode=='rep':conditions += [reg['rcx']==4096,reg['rsi']==0x51000,reg['rdi']==0x61000,reg['rflags']==70,r['rep_bytes_valid']]
   elif mode=='linux':
    progress=target%1_000_000;left=1_000_000-progress;flags=0x202|(4 if (left&255).bit_count()%2==0 else 0)|(16 if left&15==15 else 0)
    conditions += [reg['rax']==progress,reg['rcx']==left,reg['rflags']==flags,r['input_state']['manifest_sha256']=='571b39767430b5ecaba9bbcbabfdb1b1ea89600bb13003cb4b8444706f065f8c']
    if target==1_000_100:conditions += [any(b['tag']==3 and b['count']==1_000_000 and b['phase']=='correction' and b['completion']['before_count']==b['completion']['after_count'] and b['completion']['after_registers']['rip']==0x4010a8 for b in r['pio_boundaries'])]
   else:
    left=(loops-target)&0xffffffff;flags=2|(4 if (left&255).bit_count()%2==0 else 0)|(16 if left&15==15 else 0)|(64 if left==0 else 0)|(128 if left&(1<<31) else 0)|(2048 if left==0x7fffffff else 0)
    conditions += [reg['r8']==7*target,reg['rcx']==left,reg['rflags']==flags]
   if mode=='exits':
    w=r['pio_witness'];conditions += [w is not None,any(b.get('phase')=='correction' and b['tag']==3 for b in r['pio_boundaries'])]
    if w:
     c=w['completion'];conditions += [c['requested_immediate_exit']==1,c['before_count']==c['after_count'],c['before_ram_sha256'] is not None,c['before_ram_sha256']==c['after_ram_sha256'],c['debug_completion'],w['canary_before']==w['canary_after'],w.get('following_instruction',{}).get('canary')==w['canary_before']+1]
   if not all(conditions):issues.append({'evidence':q['evidence'],'failed_condition_indices':[i for i,c in enumerate(conditions) if not c]})
  ref_rows=refs.get(key[:-1],[])
  if not ref_rows:issues.append('fresh uninterrupted terminal reference missing')
  else:
   fields=('count','registers_sha256','ram_sha256','debug_registers','vcpu_events')
   for q in rs:
    t=q['row'].get('terminal')
    if t is None:continue
    if not any(z['exitcode']==0 and z['guard_exit']==0 and z['row']['terminal']['time_enabled_ns']>0 and z['row']['terminal']['time_enabled_ns']==z['row']['terminal']['time_running_ns'] and z['row']['terminal']['debug_registers'].startswith('Ok(') and z['row']['terminal']['vcpu_events'].startswith('Ok(') and all(t[f]==z['row']['terminal'][f] for f in fields) and z['row']['initial_ram_sha256']==q['row']['initial_ram_sha256'] and z['row']['initial_registers']==q['row']['initial_registers'] and owner_inventory(z['row']) and z['row'].get('input_state')==q['row'].get('input_state') for z in ref_rows):issues.append('terminal differs from uninterrupted reference in '+q['evidence'])
  accepted=not issues
 entry={'profile':key[:-4],'mode':mode,'target':target,'margin':margin,'kind':kind,'runs':len(rs),'distinct_tids':len(tids),'initial_variants':len(initials),'landing_variants':len(identities),'accepted':accepted,'issues':issues,'counts':sorted({r['final_count'] for r in rows}),'step_range':[min(r['correction_steps'] for r in rows),max(r['correction_steps'] for r in rows)],'elapsed_ns_range':[min(r['elapsed_ns'] for r in rows),max(r['elapsed_ns'] for r in rows)],'evidence':[r['evidence'] for r in rs]};summary.append(entry)
 if not accepted:failures.append({'profile':key,'reason':issues or 'control requirements not met'})
print(json.dumps({'probe_sha256':a.probe,'guest_sha256':a.guest,'records':len(records),'groups':summary,'failures':failures,'accepted':bool(summary) and not failures},indent=2))
raise SystemExit(0 if summary and not failures else 1)
