"""Analyze three frozen exploratory attempts, preserving failed stops as failures."""
import ast,hashlib,json,pathlib
BASE=pathlib.Path(__file__).resolve().parent
BATCH=BASE/'cost-exploratory-margin-v1'
plan=json.loads((BASE/'next-experiment-proposal.json').read_text())
assert json.loads((BATCH/'expected-matrix.json').read_text())==plan
assert len(plan['attempts'])==3
ledger=[json.loads(s) for s in (BATCH/'ledger.jsonl').read_text().splitlines()]
assert [(x['id'],x['state']) for x in ledger]==[(a['id'],s) for a in plan['attempts'] for s in ['started','finished']]
tree=ast.parse((BASE/'source/host_profile.py').read_text())
expected=next(ast.literal_eval(n.value) for n in tree.body if isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='expected' for t in n.targets))
for n in ['host-before','host-after']:
 host=json.loads((BATCH/(n+'.json')).read_text());assert host['profile']==expected and host['matches_pinned_profile']
for p,sha in plan['identities'].items():
 if not p.startswith('target/'):assert hashlib.sha256((BASE/'source'/p).read_bytes()).hexdigest()==sha
prior=BASE.parent/'preemption-v4'
landing=json.loads((prior/'run.usKaoQ/stdout.txt').read_text())
reference=json.loads((prior/'run.cjHvpK/stdout.txt').read_text())
records=[]
for a in plan['attempts']:
 d=BATCH/a['id'];r=json.loads((BASE/(a['id']+'.json')).read_text());v=json.loads((d/'stdout.txt').read_text())
 code=int((d/'exit-code.txt').read_text())
 assert next(x for x in ledger if x['id']==a['id'] and x['state']=='finished')['exit_code']==code==r['child_exit']
 assert r['mode']=='original' and r['child_args']==list(map(str,a['args']))
 assert r['reviewed_probe_sha256']==plan['identities']['target/release/preempt'] and r['guard_sha256']==plan['identities']['msr_guard.py']
 assert r['original']==r['restored']=={'2':'0x4480000000000','18':'0x4480000000000'}
 assert not r['errors'] and not r['applied'] and not r.get('timed_out')
 assert v['perf_config']==0x5100d1 and v['perf_flags']==524293 and v['perf_valid'] and v['time_enabled_ns']==v['time_running_ns']>0
 assert v['host_admin_count_before']==v['host_admin_count_after']==0 and v['watchdog_alarms']==0 and v['signals']==1
 mode,target,margin,method=a['args']
 assert v['target']==target and v['margin']==margin and v['correction_steps']==v['debug_exits']
 if v['accepted']:
  assert code==0 and v['landed'] and not v['overshoot'] and v['final_count']==target
  for k in ['final_registers','registers_sha256','ram_sha256','debug_registers','vcpu_events']:assert v[k]==landing[k],k
  for k in ['count','registers','registers_sha256','ram_sha256','debug_registers','vcpu_events','analytic_valid']:assert v['terminal'][k]==reference['terminal'][k],k
  assert len(v['debug_records'])==20
  if method=='breakpoint':
   assert v['debug_exits']==target-v['overflow_count']
   assert all(x['exception']==1 and x['pc']==40970 and x['dr6']&1 and not x['dr6']&(1<<14) for x in v['debug_records'])
  else:
   assert method=='count_only' and all(x['exception']==1 and x['dr6']&(1<<14) and not x['dr6']&1 for x in v['debug_records'])
 else:
  assert code==2 and v['overshoot'] and not v['landed'] and v['final_count']==v['overflow_count']>target
  assert v['terminal'] is None and v['debug_exits']==0
 records.append({'id':a['id'],'method':v['correction_method'],'target':target,'margin':margin,'status':'valid_exact_stop' if v['accepted'] else 'rejected_overshoot','raw_exit_code':code,'overflow_count':v['overflow_count'],'skid':v['overflow_count']-(target-margin),'final_count':v['final_count'],'overshoot_branches':max(0,v['final_count']-target),'remaining_branches_at_overflow':max(0,target-v['overflow_count']),'correction_exits':v['debug_exits'],'timing_ns':{k:v[k] for k in ['normal_stop_overhead_ns','correction_ns','overflow_elapsed_ns','landing_witness_ns','setup_ns','initial_witness_ns','continuation_total_ns']},'eligible_for_success_cost_summary':v['accepted']})
report={
 'scope':'three one-process original-profile idle exploratory attempts; not a median qualification',
 'planned':3,'measured':3,'all_receipts_and_host_pins_valid':True,'exact_successes':2,'rejected_overshoots':1,'records':records,
 'successful_stop_bracket_us':[x['timing_ns']['normal_stop_overhead_ns']/1000 for x in records if x['eligible_for_success_cost_summary']],
 'interpretation':'margin reduction cuts thousands of correction exits, but both successful stops still exceed the100us budget by over50x; margin1500 failed before correction, so its16.501us bracket is not a valid fast stop',
 'comparison_limit':'breakpoint and count-only overflowed at different counts; their5.30ms and5.19ms totals do not establish equal-tail relative speed',
 'timing_scope':'normal_stop_overhead_ns starts after overflow returns and includes debug-off/post-counter/scalar validation; signal-return latency excluded. Witness, setup and actual continuation are separately bracketed; no complete overhead or normal-mode throughput claim',
 'next_step':'No five-process repeats of these configurations. One Linux target1100000/margin4000/count_only smoke would test the generic count-directed primitive on the already pinned Linux snapshot with no PIO in its correction tail. If it remains millisecond-scale, stop margin tuning and design bounded correction/trap cost reduction before a deciding gate matrix.'
}
(BASE/'margin-analysis.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps(report,indent=2))
