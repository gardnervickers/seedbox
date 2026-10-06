#!/usr/bin/env python3
"""Measurement arithmetic/guards. Temporary stdlib analysis, not a world runtime."""
import argparse, hashlib, json, math, pathlib

BUCKETS=('guest_run_ns','natural_exit_handling_ns','precise_stop_ns','world_device_ns','witness_ns','other_ns')

def integer(value, name, positive=False):
    if type(value) is not int or value < (1 if positive else 0):
        raise ValueError(name+' must be an integer'+('>0' if positive else '>=0'))
    return value

def check_positive(value, name):
    if type(value) not in (int,float) or not math.isfinite(value) or value <= 0:
        raise ValueError(name+' must be finite>0')
    return value

def evaluate(data, profile, profile_hash):
    if data['schema_version'] != 1 or data['profile_id'] != profile['profile_id']:
        raise ValueError('schema/profile mismatch')
    q=data['qualification']
    if q not in ('unmeasured','local_smoke','exploratory','deciding','synthetic_test'):
        raise ValueError('unknown qualification')
    if data['mode'] not in ('normal','check'):
        raise ValueError('unknown witness mode')
    if q=='unmeasured':
        return {'qualification':'unmeasured','mode':data['mode'],'decision':'not_run','targets':None,
                'note':'null measurements are neither zero-cost nor passes'}
    if data['profile_sha256'] != profile_hash:
        raise ValueError('workload assumptions/profile bytes changed')
    t=data['timing'];w=profile['workload'];m=data['metrics'];c=data['calibration'];k=data['correctness']
    fixed=integer(t['fixed_workload_ns'],'fixed_workload_ns',True)
    if fixed!=w['duration_ns']:raise ValueError('fixed horizon differs fromprofile')
    host=integer(t['end_to_end_host_ns'],'end_to_end_host_ns',True)
    active_host=integer(t['active_host_ns'],'active_host_ns',True)
    if active_host>host:raise ValueError('activehost>end-to-endhost')
    drain=integer(t['drain_sim_ns'],'drain_sim_ns')
    simulated=integer(t['actual_sim_duration_ns'],'actual_sim_duration_ns',True)
    if simulated!=fixed+drain:raise ValueError('actual sim duration must include actual recovery/check drain')
    buckets={b:integer(data['exclusive_host_ns'][b],b) for b in BUCKETS}
    accounted=sum(buckets.values());error=abs(accounted-host)
    if error*100>host:raise ValueError('exclusivebucket sum differs fromhostwall by>1%; possible doublecount/omission')
    if c['qualification']!='measured' or c['profile_id']!=data['profile_id']:
        raise ValueError('missing measured calibration for workloadprofile')
    if c['counter_profile_id']!=data['host']['counter_profile_id'] or not c['counter_profile_id']:
        raise ValueError('calibrationcounter differs fromguestcounter')
    cpu=integer(c['cpu_execution_ns'],'nativecpu_execution_ns',True)
    count=integer(c['conditional_branches'],'nativeconditionalbranches',True)
    num=integer(c['ns_per_branch_numerator'],'ns_per_branch_numerator',True)
    den=integer(c['ns_per_branch_denominator'],'ns_per_branch_denominator',True)
    if num*count!=cpu*den:raise ValueError('ns/branch is not calibrated nativeCPU/count; idle walltime is forbidden')
    enabled=integer(c['perf_time_enabled_ns'],'perf enabled',True)
    running=integer(c['perf_time_running_ns'],'perf running',True)
    if enabled!=running:raise ValueError('multiplexing/disabledtime needs explicit qualified correction; not accepted here')
    for field in ('kernel_release','kernel_build_id','cpu_model','microcode','counter_profile_id'):
        if not isinstance(data['host'][field],str) or not data['host'][field]:raise ValueError('missing host pin '+field)
    if data['host']['pin_before_after_match'] is not True:raise ValueError('host kernel/microcode/counter profile changed or unverified')
    expected_writes=(w['operations']+w['write_every_n']-1)//w['write_every_n']
    expected={'offered_operations':w['operations'],'reads':w['operations']-expected_writes,
              'acknowledged_writes':expected_writes,'replica_keys_verified':w['key_count']}
    for field,value in expected.items():
        if integer(k[field],field)!=value:raise ValueError('workload count mismatch '+field)
    for field in ('client_pass','aof_rewrite_completed','partition_observed','healed_delivery_observed'):
        if k[field] is not True:raise ValueError('missing benchmark oracle/effect '+field)
    if integer(k['partition_dropped_frames'],'partition_dropped_frames',True)<1 or integer(k['writes_during_partition'],'writes_during_partition',True)<1:
        raise ValueError('partition never reached workload')
    if integer(k['actual_stop_overshoots'],'actual_stop_overshoots')!=0:raise ValueError('actual stop overshoot')
    targets=profile['measurement']
    gate={
        'fixed60s_end_to_end_speed':fixed/host>=targets['speed_target'],
        'drain_within_limit':drain<=w['recovery_drain_limit_ns'],
        'busy_slowdown':check_positive(m['busy_slowdown'],'busy slowdown')<=targets['busy_slowdown_max'],
        'wakeup_cost':check_positive(m['wakeup_cost_mean_ns'],'wakeup mechanismcost')<=targets['host_cost_per_wakeup_max_ns'],
        'natural_exit_cost':check_positive(m['natural_exit_roundtrip_p50_ns'],'naturalexit roundtrip')<=targets['natural_exit_median_max_ns'],
        'precise_stop_cost':check_positive(m['precise_stop_p50_ns'],'precise stop')<=targets['precise_stop_median_max_ns']
    }
    integer(m['wakeup_episodes'],'actual wakeupepisodes',True)
    integer(m['precise_stop_samples'],'precise_stop_samples',True)
    util=check_positive(c['native_cpu_utilization_sum'],'native summedutilization')
    if util>len(profile['machines']):raise ValueError('summedutilization exceeds declaredCPU count')
    qualify=data['deciding_qualification']
    decided=(q=='deciding' and integer(qualify['fresh_processes'],'fresh processes')>=5 and
             qualify['witness_match'] is True and qualify['independent_review'] is True and bool(data['input_manifest']))
    return {'qualification':q,'mode':data['mode'],
            'decision':('targets_met' if all(gate.values()) else 'targets_failed') if decided else 'not_qualified',
            'targets':gate,'simulated_per_host':{'active':fixed/active_host,'including_drain':simulated/host,'fixed60s_end_to_end':fixed/host},
            'host_accounting':{'sum_ns':accounted,'host_wall_ns':host,'error_ns':error,'exclusive':buckets},
            'calibrated_ns_per_conditional_branch':num/den,
            'native_cpu_utilization_sum':util,
            'model_busy_host_seconds_per_sim_second':util*m['busy_slowdown'],
            'model_note':'add only nonoverlapping wakeup/stop/world/witness residuals; measured end-to-end ratio remains deciding',
            'limits':data['limits']}

def main():
    ap=argparse.ArgumentParser();ap.add_argument('input',type=pathlib.Path);ap.add_argument('--profile',type=pathlib.Path)
    ap.add_argument('--output',type=pathlib.Path);args=ap.parse_args()
    profile_path=args.profile or pathlib.Path(__file__).resolve().parents[1]/'profile.json'
    raw=profile_path.read_bytes();profile=json.loads(raw)
    data=json.loads(args.input.read_text(),parse_constant=lambda x:(_ for _ in ()).throw(ValueError('non-finiteJSON '+x)))
    result=evaluate(data,profile,hashlib.sha256(raw).hexdigest())
    encoded=json.dumps(result,indent=2,allow_nan=False)+'\n'
    if args.output:args.output.write_text(encoded)
    print(encoded,end='')

if __name__=='__main__':main()
