"""Off-host inspection of the frozen one-attempt breakpoint smoke; no gate claim."""
import ast,hashlib,json,pathlib
BASE=pathlib.Path(__file__).resolve().parent
batch=BASE/'cost-smoke-breakpoint-v1'
plan=json.loads((BASE/'smoke-plan.json').read_text())
assert json.loads((batch/'expected-matrix.json').read_text())==plan
assert len(plan['attempts'])==1
attempt=plan['attempts'][0];d=batch/attempt['id']
ledger=[json.loads(s) for s in (batch/'ledger.jsonl').read_text().splitlines()]
assert [(x['id'],x['state']) for x in ledger]==[(attempt['id'],'started'),(attempt['id'],'finished')]
assert ledger[-1]['exit_code']==int((d/'exit-code.txt').read_text())==0
receipt=json.loads((BASE/(attempt['id']+'.json')).read_text())
assert receipt['mode']=='original' and receipt['child_args']==list(map(str,attempt['args']))
assert receipt['reviewed_probe_sha256']==plan['identities']['target/release/preempt']
assert receipt['guard_sha256']==plan['identities']['msr_guard.py']
assert receipt['original']==receipt['restored']=={'2':'0x4480000000000','18':'0x4480000000000'}
assert receipt['applied']=={} and not receipt['errors'] and not receipt.get('timed_out') and receipt['child_exit']==0
for p,sha in plan['identities'].items():
 if not p.startswith('target/'):
  assert hashlib.sha256((BASE/'source'/p).read_bytes()).hexdigest()==sha
tree=ast.parse((BASE/'source/host_profile.py').read_text())
expected=next(ast.literal_eval(n.value) for n in tree.body if isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='expected' for t in n.targets))
for name in ['host-before','host-after']:
 host=json.loads((batch/(name+'.json')).read_text())
 assert host['profile']==expected and host['matches_pinned_profile']
v=json.loads((d/'stdout.txt').read_text())
assert v['accepted'] and v['landed'] and v['analytic_valid'] and not v['overshoot']
assert v['final_count']==100000 and v['final_registers']['rip']==40970
assert v['final_registers']['r8']==700000 and v['final_registers']['rcx']==100000 and v['final_registers']['rflags']==6
assert v['perf_valid'] and v['time_enabled_ns']==v['time_running_ns']>0
assert v['perf_config']==0x5100d1 and v['perf_flags']==524293 and v['host_admin_count_before']==v['host_admin_count_after']==0
assert v['signals']==1 and v['watchdog_alarms']==0 and v['debug_exits']==v['correction_steps']==100000-v['overflow_count']
assert len(v['debug_records'])==20
assert all(x['exception']==1 and x['pc']==40970 and x['dr6']&1 and not x['dr6']&(1<<14) and x['dr7']==0x401 for x in v['debug_records'])
assert [x['count'] for x in v['debug_records']]==list(range(v['overflow_count']+1,v['overflow_count']+21))
assert v['debugger_resume_flag_cleanup']['before']==v['debugger_resume_flag_cleanup']['after']==6
prior=BASE.parent/'preemption-v4'
def raw(name):return json.loads((prior/name/'stdout.txt').read_text())
landing=raw('run.usKaoQ');reference=raw('run.cjHvpK')
for k in ['final_registers','registers_sha256','ram_sha256','debug_registers','vcpu_events']:
 assert v[k]==landing[k],k
for k in ['count','registers','registers_sha256','ram_sha256','debug_registers','vcpu_events','analytic_valid']:
 assert v['terminal'][k]==reference['terminal'][k],k
report={
 'scope':'one exploratory original-profile bare known-RIP breakpoint smoke; no fresh-process or performance gate qualification',
 'attempt_id':attempt['id'],'raw_accepted':True,'analytic_and_prior_state_match':True,
 'landing_reference':'../preemption-v4/run.usKaoQ/stdout.txt','terminal_reference':'../preemption-v4/run.cjHvpK/stdout.txt',
 'overflow_count':v['overflow_count'],'requested_period':90000,'observed_skid':v['overflow_count']-90000,
 'correction_branches':100000-v['overflow_count'],'actual_debug_traps':v['debug_exits'],
 'first_20_traps':'DR6.B0 set, DR6.BS clear, one conditional branch per hardware breakpoint trap at recurring loop RIP',
 'timing_ns':{k:v[k] for k in ['setup_ns','initial_witness_ns','overflow_elapsed_ns','correction_ns','normal_stop_overhead_ns','landing_witness_ns','continuation_total_ns']},
 'timing_definitions':{
  'normal_stop_overhead_ns':'return from overflow through correction, debug-off/RF cleanup, post-counter and scalar getters/validation; excludes overflow signal/KVM-return latency and witnesses',
  'overflow_elapsed_ns':'guest execution to overflow plus signal/KVM-return administration, inseparable in this bracket',
  'landing_witness_ns':'register JSON serialization plus RAM hash; final register SHA computed outside bracket',
  'continuation_total_ns':'remaining guest execution plus terminal PIO and terminal full-state witness',
  'setup_ns':'VM/counter setup, separate from initial witness'},
 'descriptive_ns_per_debug_trap':v['normal_stop_overhead_ns']/v['debug_exits'],
 'ratio_of_observed_correction_bracket_to_100us_budget':v['normal_stop_overhead_ns']/100000,
 'performance_gate':'open; this one correction bracket alone exceeds budget by 631x; median not measured',
 'limits':['future target RIP and original guest RF=0 are fixture assumptions','first 20 trap details logged, total trap count logged','all witnesses still executed; split timings do not implement a normal witness mode','no Linux breakpoint/guest debug-IRQ transparency qualification'],
}
(BASE/'smoke-analysis.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps(report,indent=2))
