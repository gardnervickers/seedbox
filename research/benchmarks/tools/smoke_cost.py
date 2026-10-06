#!/usr/bin/env python3
"""Synthetic arithmetic sensitivity smoke; never produces benchmark acceptance."""
import copy, hashlib, json, pathlib, sys
sys.dont_write_bytecode=True
from cost_report import evaluate

root=pathlib.Path(__file__).resolve().parents[1]
raw=(root/'profile.json').read_bytes();profile=json.loads(raw);digest=hashlib.sha256(raw).hexdigest()
template=json.loads((root/'measurement.template.json').read_text())
assert evaluate(template,profile,digest)['decision']=='not_run'
data=copy.deepcopy(template);data.update(qualification='synthetic_test',profile_sha256=digest,input_manifest='synthetic-none')
data['host'].update(kernel_release='synthetic',kernel_build_id='synthetic',cpu_model='synthetic',microcode='synthetic',counter_profile_id='synthetic',pin_before_after_match=True)
data['calibration'].update(qualification='measured',profile_id=profile['profile_id'],counter_profile_id='synthetic',cpu_execution_ns=100000000,conditional_branches=5000000,ns_per_branch_numerator=20,ns_per_branch_denominator=1,perf_time_enabled_ns=60000000000,perf_time_running_ns=60000000000,native_cpu_utilization_sum=.01,coverage='synthetic inputs only')
data['timing'].update(actual_sim_duration_ns=60100000000,drain_sim_ns=100000000,active_host_ns=2900000000,end_to_end_host_ns=3000000000)
data['exclusive_host_ns'].update(guest_run_ns=1000000000,natural_exit_handling_ns=500000000,precise_stop_ns=500000000,world_device_ns=400000000,witness_ns=100000000,other_ns=500000000)
data['metrics'].update(busy_slowdown=2,wakeup_cost_mean_ns=40000,wakeup_episodes=100,natural_exit_roundtrip_p50_ns=8000,precise_stop_p50_ns=50000,precise_stop_samples=100)
data['correctness'].update(client_pass=True,client_outcome_sha256='synthetic',offered_operations=6000,reads=4800,acknowledged_writes=1200,replica_keys_verified=1024,aof_rewrite_completed=True,partition_observed=True,partition_dropped_frames=200,writes_during_partition=200,healed_delivery_observed=True,actual_stop_overshoots=0)
data['deciding_qualification'].update(fresh_processes=5,witness_match=True,independent_review=True)
result=evaluate(data,profile,digest)
assert result['decision']=='not_qualified' and all(result['targets'].values())
bad=copy.deepcopy(data);bad['exclusive_host_ns']['precise_stop_ns']+=500000000
try:evaluate(bad,profile,digest)
except ValueError as e:assert 'doublecount' in str(e)
else:raise AssertionError('doublecount not rejected')
bad=copy.deepcopy(data);bad['calibration']['ns_per_branch_numerator']=1
try:evaluate(bad,profile,digest)
except ValueError as e:assert 'calibrated' in str(e)
else:raise AssertionError('arbitrary branch conversion not rejected')
bad=copy.deepcopy(data);bad['profile_sha256']='0'*64
try:evaluate(bad,profile,digest)
except ValueError as e:assert 'changed' in str(e)
else:raise AssertionError('silently changedprofile accepted')
bad=copy.deepcopy(data);bad['correctness']['partition_dropped_frames']=0
try:evaluate(bad,profile,digest)
except ValueError:pass
else:raise AssertionError('nonreachedfault accepted')
print(json.dumps({'qualification':'synthetic arithmetic smoke only','unmeasured_is_not_run':True,'synthetic_cannot_qualify':True,'doublecount_rejected':True,'uncalibrated_factor_rejected':True,'profile_change_rejected':True,'unreached_fault_rejected':True},indent=2))
