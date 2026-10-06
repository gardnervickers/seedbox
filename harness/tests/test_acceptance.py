"""Acceptance suite logic with a fake launcher and a seedbox CLI stub; never invokes KVM, sudo or MSRs."""
import json
import os
from pathlib import Path
import sys
import tempfile
import signal
import subprocess
import time
import unittest
from unittest.mock import patch

SIM = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SIM))

from acceptance import checks  # noqa: E402
from acceptance.engine import execute, overall  # noqa: E402
from acceptance.launcher import CliLauncher  # noqa: E402
from acceptance.model import FAIL, FULL_STATE, PASS, UNRESOLVED, Job, RunResult  # noqa: E402

CASES = json.loads((SIM / "acceptance/cases.json").read_text())


class FakeLauncher:
    """Deterministic world: the witness depends only on scenario, seed and witness mode."""

    def __init__(self, tweak=None):
        self.tweak = tweak or (lambda job, result, n: None)
        self.jobs = []

    def run(self, jobs):
        results = []
        for job in jobs:
            self.jobs.append(job)
            base = f"{job.scenario}/{job.seed}"
            status, line = PASS, "RESULT pass ok"
            if job.scenario in ("sanity", "kill-3.5.2"):
                status, line = FAIL, "RESULT fail missing"
            result = RunResult(job, status, result_line=line, workload_exit=int(status == FAIL),
                               world_witness=f"w:{base}",
                               machine_witnesses={"n1": f"n1:{base}", "client": f"c:{base}"},
                               state_digests={"n1": f"d:{base}"} if job.witness == FULL_STATE else {},
                               host_seconds=1.0, sim_seconds=10.0,
                               network_counters={"n1->n2": {"partition_dropped": 2}},
                               faults=[{"action": action, "service": "n1", "requested_at_ns": i, "applied_at_ns": i + 1} for i, action in enumerate(("stop", "restart", "partition", "heal"))]
                               if "kill" in job.scenario else [])
            if job.scenario == "race":
                lost = job.seed % 3
                result.logs = {"n1": f"RACE mode=racy counter=9 expected=9 lost={lost}\n"
                                     "RACE mode=atomic counter=9 expected=9 lost=0\n"}
                result.result_line, result.workload_exit = None, 0
            self.tweak(job, result, len(self.jobs))
            results.append(result)
        return results


def verdicts(launcher, tier=checks.FULL, **ctx):
    outcomes = execute(tier, checks.Context(CASES, **ctx), launcher)
    return {o.name.split()[0]: o.verdict for o in outcomes}, outcomes


class CheckTests(unittest.TestCase):
    def test_deterministic_build_passes_and_shares_default_runs(self):
        launcher = FakeLauncher()
        result, outcomes = verdicts(launcher)
        self.assertEqual({k: v.status for k, v in result.items()},
                         {"1": PASS, "2": PASS, "3": PASS, "4": PASS, "8": PASS, "6": PASS, "7": checks.SKIPPED})
        self.assertEqual(overall(outcomes), "PASS")
        short = [j for j in launcher.jobs if j.scenario == "short-kill-3.5.2"]
        # Items 1 (2 + 2 digest runs), 2 (3 variants) and 8 share one default run.
        self.assertEqual(len(short), 7)
        self.assertIn("RAM/register digests", result["1"].summary)
        self.assertEqual(len([j for j in launcher.jobs if j.scenario == "kill-3.5.3"]), 11)

    def test_replay_digest_mismatch(self):
        def tweak(job, result, n):
            if job.witness == FULL_STATE and n % 2:
                result.state_digests["n1"] = "other"
        result, _ = verdicts(FakeLauncher(tweak), checks.SMOKE)
        self.assertEqual(result["1"].status, FAIL)
        self.assertIn("n1:digest", result["1"].summary)

    def test_stop_placement_difference_is_a_determinism_bug(self):
        def tweak(job, result, n):
            if ("undo", "0") in job.performance:
                result.machine_witnesses["client"] = "skewed"
        result, outcomes = verdicts(FakeLauncher(tweak), checks.SMOKE)
        self.assertEqual(result["2"].status, FAIL)
        self.assertIn("undo=0 differs in client", result["2"].summary)
        self.assertEqual(overall(outcomes), "REGRESSION")

    def test_race_needs_varied_outcomes_clean_atomic_and_a_replaying_failure(self):
        def same(job, result, n):
            if job.scenario == "race":
                result.logs["n1"] = result.logs["n1"].replace(f"racy counter=9 expected=9 lost={job.seed % 3}",
                                                              "racy counter=5 expected=9 lost=4")
        self.assertIn("same outcome", verdicts(FakeLauncher(same), checks.SMOKE)[0]["3"].summary)

        def dirty(job, result, n):
            if job.scenario == "race" and job.seed == 2:
                result.logs["n1"] = result.logs["n1"].replace("atomic counter=9 expected=9 lost=0", "atomic counter=8 expected=9 lost=1")
        self.assertIn("atomic control lost 1", verdicts(FakeLauncher(dirty), checks.SMOKE)[0]["3"].summary)

        launcher = FakeLauncher()
        def diverge(job, result, n):
            if job.scenario == "race" and len([j for j in launcher.jobs if j.scenario == "race"]) > len(CASES["race"]["seeds"]):
                result.world_witness = "diverged"
        launcher.tweak = diverge
        result = verdicts(launcher, checks.SMOKE)[0]["3"]
        self.assertEqual(result.status, FAIL)
        self.assertIn("did not replay", result.summary)

    def test_pinned_margin_completed_only_by_retry_was_not_exercised(self):
        def retried(job, result, n):
            if ("margin", "32768") in job.performance:
                result.retries, result.attempts = 1, [{"evidence": "x", "detail": "exit status 1: overshoot on n1"}]
        result = verdicts(FakeLauncher(retried), checks.SMOKE)[0]["2"]
        self.assertEqual(result.status, UNRESOLVED)
        self.assertIn("margin=32768 not exercised", result.summary)

        def also_diverges(job, result, n):
            retried(job, result, n)
            if ("undo", "0") in job.performance:
                result.world_witness = "skewed"
        # A real determinism failure is never hidden behind an unexercised margin.
        result = verdicts(FakeLauncher(also_diverges), checks.SMOKE)[0]["2"]
        self.assertEqual(result.status, FAIL)
        self.assertIn("undo=0 differs", result.summary)

    def test_fault_effects_need_applied_faults_and_recovery(self):
        def none(job, result, n):
            if job.scenario == "short-kill-3.5.2":
                result.faults = []
        self.assertIn("missing confirmed", verdicts(FakeLauncher(none), checks.SMOKE)[0]["8"].summary)

        def scheduled_only(job, result, n):
            result.faults = [f for f in result.faults if f["action"] != "restart"]
        result = verdicts(FakeLauncher(scheduled_only), checks.SMOKE)[0]["8"]
        self.assertEqual(result.status, FAIL)
        self.assertIn("missing confirmed", result.summary)

        def undropped(job, result, n):
            result.network_counters = {}
        self.assertEqual(verdicts(FakeLauncher(undropped), checks.SMOKE)[0]["8"].status, FAIL)

        def broken(job, result, n):
            if job.scenario == "short-kill-3.5.2":
                result.status, result.detail = FAIL, "RESULT fail missing"
        self.assertIn("did not recover", verdicts(FakeLauncher(broken), checks.SMOKE)[0]["8"].summary)

        def unfaulted(job, result, n):
            if job.scenario == "kill-3.5.3" and job.seed == 4002:
                result.faults = []
        self.assertIn("seeds [4002]", verdicts(FakeLauncher(unfaulted))[0]["6"].summary)

    def test_launcher_error_is_unresolved_not_a_crash(self):
        class Broken:
            def run(self, jobs):
                raise OSError("batch.sh: permission denied")
        result, outcomes = verdicts(Broken(), checks.SMOKE)
        self.assertEqual({v.status for v in result.values()}, {UNRESOLVED})
        self.assertIn("permission denied", result["1"].summary)
        self.assertEqual(overall(outcomes), "UNRESOLVED")

    def test_checker_control_must_fail_with_exit_one(self):
        def passes(job, result, n):
            if job.scenario == "sanity":
                result.status, result.result_line, result.workload_exit = PASS, "RESULT pass", 0
        self.assertEqual(verdicts(FakeLauncher(passes), checks.SMOKE)[0]["4"].status, FAIL)

    def test_fixed_control_failures_and_unresolved(self):
        def stuck(job, result, n):
            if job.scenario == "kill-3.5.3" and job.seed == 4003:
                result.status, result.detail = UNRESOLVED, "overshoot again"
        result, outcomes = verdicts(FakeLauncher(stuck))
        self.assertEqual(result["6"].status, UNRESOLVED)
        self.assertEqual(overall(outcomes), "UNRESOLVED")

        def fails(job, result, n):
            if job.scenario == "kill-3.5.3" and job.seed == 4007:
                result.status = FAIL
        self.assertIn("seeds [4007]", verdicts(FakeLauncher(fails))[0]["6"].summary)

    def test_cross_build_compares_declared_performance_only_change(self):
        launcher = FakeLauncher()
        result, _ = verdicts(launcher, performance_only=True)
        self.assertEqual(result["7"].status, PASS)
        baseline = [j for j in launcher.jobs if j.engine == checks.BASELINE]
        self.assertEqual({(j.scenario, j.seed) for j in baseline}, {("short-kill-3.5.2", 4000)})

        def drift(job, result, n):
            if job.engine == checks.BASELINE and job.scenario == "short-kill-3.5.2":
                result.machine_witnesses["n1"] = "old"
        result, _ = verdicts(FakeLauncher(drift), performance_only=True)
        self.assertEqual(result["7"].status, FAIL)
        self.assertIn("short-kill-3.5.2 seed 4000 differs in n1", result["7"].summary)


STUB = r'''#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys, uuid
op, *args = sys.argv[1:]
if op == 'inspect':
    print((pathlib.Path(args[0]) / 'record.json').read_text())
    sys.exit()
def opt(key, default=None):
    return args[args.index(key)+1] if key in args else default
state = pathlib.Path(opt('--state'))
state.mkdir(exist_ok=True)
with (state / 'calls.jsonl').open('a') as calls:
    calls.write(json.dumps({'op':op,'args':args,'ambient':os.environ.get('SEEDBOX_UNDO')})+'\n')
previous = json.loads((pathlib.Path(args[0])/'record.json').read_text()) if op == 'replay' else None
seed = previous['identity']['seed'] if previous else int(opt('--seed'))
full = bool(previous['report']['full_state']) if previous else '--full-state' in args
path = state / str(uuid.uuid4())
(path / 'output/machines').mkdir(parents=True)
mode = os.environ.get('STUB_MODE','')
if mode == 'sleep':
    import signal, time
    def stop(*_):
        time.sleep(0.1)
        (state/'cleaned').write_text('yes')
        sys.exit(0)
    signal.signal(signal.SIGTERM,stop)
    (state/'child.pid').write_text(str(os.getpid()))
    while True: time.sleep(0.1)
status = 'FAIL' if mode == 'failure' else 'PASS'
report = {'reason':'COMPLETED','workload_exit':{'service':'client','code':int(status == 'FAIL')},
          'witness':f'w{seed}', 'full_state':{'client':'digest'} if full else None,'faults':[]}
world = {'machines':[{'name':'client','witness':f'm{seed}','exits':5}],'sim_seconds':10}
text = 'RESULT '+status.lower()+' acked=100 ok\n'
error = None
files = {'vmm.json':json.dumps(world),'machines/client.console':text}
if mode in ('overshoot','clock','restore_overshoot') and (mode != 'overshoot' or opt('--margin') != '98304'):
    status,report = 'ERROR',None
    error = 'execution: MSR restoration failed' if mode == 'restore_overshoot' else 'execution: VMM failed; see vmm.stderr'
    files['vmm.stderr'] = 'clock bound exceeded: overshoot is not retryable here' if mode == 'clock' else 'soft overshoot on n1'
if mode == 'lease' and not (state/'lease-seen').exists():
    (state/'lease-seen').touch()
    status,report,error = 'ERROR',None,'execution: CPU lease unavailable for {2}: another run holds it'

logs = {}
for name, contents in files.items():
    (path/'output'/name).write_text(contents)
    logs[name] = hashlib.sha256(contents.encode()).hexdigest()
record = {'identity':{'seed':seed},'outcome':status,'report':report,'logs':logs,
          'replay':{'status':'MATCH'} if previous else None,'error':error}
(path/'record.json').write_text(json.dumps(record))
if mode == 'corrupt':
    (path/'output/vmm.json').write_text('{}')
print(status+': '+str(path))
sys.exit(2 if mode == 'cleanup_error' or status == 'ERROR' else (0 if previous else int(status == 'FAIL')))
'''


class CliLauncherTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.binary = self.root / 'seedbox'
        self.binary.write_text(STUB)
        self.binary.chmod(0o755)

    def launcher(self, **kwargs):
        return CliLauncher(self.binary, {'kill':'compose.yaml'}, [2,3,4,5], self.root,
                           {None:'/vmm','baseline':'/old-vmm'}, '/kernel', '/busybox',
                           prefix=(), log=lambda _: None, **kwargs)

    def calls(self):
        return [json.loads(line) for line in (self.root/'state/calls.jsonl').read_text().splitlines()]

    def test_run_then_replay_retains_inputs_and_separates_full_state(self):
        launcher = self.launcher()
        jobs = [Job('kill',7), Job('kill',7), Job('kill',7,FULL_STATE),
                Job('kill',7,performance=(('undo','0'),)),Job('kill',7,engine='baseline')]
        with patch.dict(os.environ, {'SEEDBOX_UNDO':'999'}):
            results = launcher.run(jobs)
        self.assertEqual([r.status for r in results], [PASS]*5)
        self.assertEqual(results[2].state_digests, {'client':'digest'})
        self.assertEqual(len({r.evidence for r in results}),5)
        calls = self.calls()
        self.assertEqual(sum(c['op']=='run' for c in calls),2)
        self.assertTrue(all(c['ambient'] is None for c in calls))
        variant = next(c for c in calls if '/old-vmm' in c['args'])
        self.assertEqual(variant['op'],'replay')
        self.assertIn('--allow-engine-change',variant['args'])
        launcher.run([Job('kill',7)])
        self.assertEqual(self.calls()[-1]['op'],'replay')

    def test_failed_workload_still_has_a_successful_matching_replay(self):
        with patch.dict(os.environ, {'STUB_MODE':'failure'}):
            results = self.launcher().run([Job('kill',7),Job('kill',7)])
        self.assertEqual([r.status for r in results],[FAIL,FAIL])
        self.assertTrue(all(r.completed for r in results))
        self.assertEqual([c['op'] for c in self.calls()],['run','replay'])

    def test_cleanup_failure_is_not_accepted_even_with_complete_output(self):
        with patch.dict(os.environ, {'STUB_MODE':'cleanup_error'}):
            result, = self.launcher().run([Job('kill',7)])
        self.assertEqual(result.status,UNRESOLVED)
        self.assertIn('exit contradicts',result.detail)

    def test_corrupt_output_is_not_accepted(self):
        with patch.dict(os.environ, {'STUB_MODE':'corrupt'}):
            result, = self.launcher().run([Job('kill',7)])
        self.assertEqual(result.status,UNRESOLVED)
        self.assertIn('hash mismatch',result.detail)

    def test_real_cli_overshoot_error_uses_verified_stderr_and_retries(self):
        with patch.dict(os.environ, {'STUB_MODE':'overshoot'}):
            result, = self.launcher().run([Job('kill',7)])
        self.assertEqual((result.status,result.retries),(PASS,1))
        self.assertIn('soft overshoot',result.attempts[0]['detail'])
        self.assertIn('98304',self.calls()[-1]['args'])

    def test_clock_bound_or_restore_error_never_retries(self):
        launcher = self.launcher()
        for mode in ('clock','restore_overshoot'):
            with self.subTest(mode=mode), patch.dict(os.environ, {'STUB_MODE':mode}):
                before = len(self.calls()) if (self.root/'state/calls.jsonl').exists() else 0
                result, = launcher.run([Job('kill',7)])
                self.assertEqual((result.status,result.retries),(UNRESOLVED,0))
                self.assertEqual(len(self.calls())-before,1)

    def test_real_cli_lease_error_is_waited_and_evidence_retained(self):
        with patch.dict(os.environ, {'STUB_MODE':'lease'}):
            result, = self.launcher(lease_wait=1).run([Job('kill',7)])
        self.assertEqual(result.status,PASS)
        self.assertEqual(len(result.attempts),1)
        self.assertIn('CPU lease unavailable',result.attempts[0]['detail'])
        self.assertTrue(Path(result.attempts[0]['evidence']).is_dir())

    def test_signal_to_harness_awaits_child_cleanup_and_stops_dispatch(self):
        code = """
import sys
from pathlib import Path
from acceptance import __main__ as entry
from acceptance.launcher import CliLauncher
from acceptance.model import Job
root=Path(sys.argv[1])
launcher=CliLauncher(root/'seedbox',{'kill':'compose.yaml'},[2],root,
                     {None:'/vmm'},'/kernel','/busybox',prefix=(),log=lambda _:None)
entry.suite=lambda _: launcher.run([Job('kill',i) for i in range(20)])
sys.exit(entry.main(['smoke']))
"""
        env={**os.environ,'PYTHONPATH':str(SIM),'STUB_MODE':'sleep'}
        host=subprocess.Popen([sys.executable,'-c',code,str(self.root)],env=env,
                              stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        self.addCleanup(lambda: host.poll() is None and host.kill())
        deadline=time.monotonic()+5
        while not (self.root/'state/child.pid').exists() and time.monotonic()<deadline:
            time.sleep(0.01)
        self.assertTrue((self.root/'state/child.pid').exists())
        host.send_signal(signal.SIGTERM)
        self.assertEqual(host.wait(timeout=5),130)
        self.assertTrue((self.root/'state/cleaned').exists())
        self.assertEqual(len(self.calls()),1)

    def test_default_cli_build_keeps_log_callback_callable(self):
        from acceptance import __main__ as entry
        from types import SimpleNamespace
        args=SimpleNamespace(seedbox=None,kernel=str(self.binary),artifacts=None,
                             root=str(self.root),vmm=str(self.binary),performance_only=None,
                             cpus='2',tier='smoke',lease_wait=0)
        output=json.dumps({'reason':'compiler-artifact','executable':str(self.binary),
                           'target':{'name':'seedbox','kind':['bin']}})
        with patch.dict(os.environ,{'SEEDBOX_BUSYBOX':str(self.binary)}), \
             patch.object(entry.subprocess,'run',return_value=subprocess.CompletedProcess([],0,output)):
            launcher=entry.make_launcher(args,{'kill':'compose.yaml'},self.root)
        self.assertTrue(callable(launcher.log))

    def test_cpu_lanes_exclude_smt_siblings(self):
        launcher = self.launcher(siblings=lambda cpu: {cpu//2*2,cpu//2*2+1})
        self.assertEqual(launcher.width,2)
        self.assertEqual([launcher.cpus.get(),launcher.cpus.get()],[2,4])


class ScenarioTests(unittest.TestCase):
    def test_stage_self_contained_projects_without_rewriting_yaml(self):
        from acceptance.prepare import prepare_projects
        with tempfile.TemporaryDirectory() as tmp:
            projects = prepare_projects(SIM,CASES,list(CASES['scenarios']),Path(tmp))
            for name, path in projects.items():
                self.assertEqual(Path(path).read_bytes(), (SIM/CASES['scenarios'][name]).read_bytes())
            driver = Path(tmp)/'projects/driver'
            for name in ('seedbox/control.py','etcd/main.go','race.c','etcd.py','race.py'):
                self.assertTrue((driver/name).is_file(),name)
            with self.assertRaises(FileExistsError):
                prepare_projects(SIM,CASES,list(CASES['scenarios']),Path(tmp))

    def test_guest_scenario_confirms_process_and_network_effects(self):
        from importlib.util import spec_from_file_location, module_from_spec
        sys.path.insert(0,str(SIM.parent/'python'))
        self.addCleanup(sys.path.remove,str(SIM.parent/'python'))
        spec=spec_from_file_location('acceptance_etcd',SIM/'acceptance/driver/etcd.py')
        driver=module_from_spec(spec); spec.loader.exec_module(driver)
        class World:
            def __init__(self): self.events=[]; self.active=[]
            def sleep(self,t): self.events.append(('sleep',t))
            def stop(self,target): self.events.append(('stop',target))
            def restart(self,target): self.events.append(('restart',target))
            def partition(self,left,right):
                self.events.append(('partition',left,right)); self.active=['cut']
                return type('Cut',(),{'handle':'cut'})()
            def heal(self,cut): self.events.append(('heal',cut.handle)); self.active=[]
            def inspect(self): return {'active_handles':self.active}
        class Workload:
            def poll(self): return None
        world=World(); driver.inject(world,['n1','n2','n3'],2,Workload())
        self.assertEqual([e[1] for e in world.events if e[0]=='stop'],['n1','n2'])
        self.assertEqual([e[0] for e in world.events if e[0]!='sleep'],
                         ['stop','restart','partition','heal']*2)
        self.assertEqual(driver.verify('RESULT fail acked=2 missing\n',1,True),1)
        for text,code,planted in [('RESULT pass acked=0 ok\n',0,False),
                                 ('RESULT fail acked=1 planted\n',1,True),
                                 ('RESULT fail acked=9 missing\n',0,False)]:
            with self.assertRaises(RuntimeError): driver.verify(text,code,planted)

    def test_changed_fresh_outcomes_are_not_historical_pins(self):
        def changed(job,result,n):
            result.status,result.workload_exit,result.world_witness=FAIL,1,'new'
        result,_=verdicts(FakeLauncher(changed), [('1 replay',checks.replay)])
        self.assertEqual(result['1'].status,PASS)


if __name__ == '__main__':
    unittest.main()
