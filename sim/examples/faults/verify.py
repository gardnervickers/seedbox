#!/usr/bin/env python3
"""Verify synthetic retained recovery evidence and actual acknowledged-key loss."""
import argparse
import json
from pathlib import Path
import re

RESULT = re.compile(r"^RESULT (pass|fail) acked=(\d+) pre_restart=(\d+) post_restart=(\d+) generation=(\d+) missing=(\d+)$", re.M)
READY = re.compile(r"^SERVER_READY generation=(\d+) recovered=(\d+) broken=(true|false)$", re.M)
MISSING = re.compile(r"^MISSING_ACK key=(\S+) expected=(\S+) acknowledged_generation=(\d+)$", re.M)
PHASE = re.compile(r"^ACK_PHASE generation=(\d+) acked=(\d+) total=(\d+)$", re.M)
PROBE = re.compile(r"^SCENARIO_PROBE phase=(both|partial) (.+)$", re.M)
OPERATIONS = ["stop", "restart", "partition", "partition", "heal", "heal", "stop", "restart"]

def verify(run, expected="PASS", faulted=True):
    run = Path(run)
    record = json.loads((run / "manifest.json").read_text())["record"]
    assert record["outcome"] == expected, (record["outcome"], record.get("error"))
    report = record["report"]
    assert report and report["reason"] == "COMPLETED", report
    assert report["workload_exit"] == {"service":"scenario", "code":0 if expected == "PASS" else 1}
    world = json.loads((run / "output" / "vmm.json").read_text())
    faults = world["faults"]
    assert {machine["name"] for machine in world["machines"]} == {"client", "server", "scenario"}
    assert world.get("fault_error") is None and not world.get("rejected_fault_effects")
    assert len(faults) == (8 if faulted else 0)
    assert [f["action"] for f in faults] == (OPERATIONS if faulted else [])
    for i, fault in enumerate(faults, 1):
        assert fault["id"] == i and fault["status"] == "applied", fault
        assert fault["applied_at_ns"] >= fault["requested_at_ns"], fault
        if fault["action"] in ("stop", "restart"):
            assert fault["machine"] == "server", fault
        elif fault["action"] == "partition":
            assert fault["request"]["left"] == ["client"] and fault["request"]["right"] == ["server"], fault
    transcript = world["control_history"]
    assert transcript and world["control_latency_ns"] > 0, "missing runtime control transcript"
    calls = [entry for entry in transcript if entry["request"]["op"] in OPERATIONS]
    assert [entry["request"]["op"] for entry in calls] == (OPERATIONS if faulted else []), calls
    for i, entry in enumerate(transcript, 1):
        request, response = entry["request"], entry["response"]
        assert entry["relay_id"] == i and request["id"] == i and response["id"] == request["id"] and response["ok"], entry
        times = [response[k] for k in ("requested_at_ns", "admitted_at_ns", "effect_at_ns", "replied_at_ns")]
        assert times == sorted(times), entry
        assert response["admitted_at_ns"] - response["requested_at_ns"] == world["control_latency_ns"], entry
        assert entry["delivered_at_ns"] >= response["replied_at_ns"], entry
    for call, fault in zip(calls, faults):
        assert call["response"]["effect_at_ns"] == fault["applied_at_ns"], (call, fault)
    if faulted:
        snapshots = [entry["response"]["result"] for entry in transcript if entry["request"]["op"] == "inspect"]
        handles = [entry["response"]["result"]["handle"] for entry in calls if entry["request"]["op"] == "partition"]
        assert len(handles) == 2 and handles[0] != handles[1]
        assert [snapshot["active_handles"] for snapshot in snapshots] == [handles, [handles[1]], []], snapshots
        assert all(set(snapshot["services"]) == {"client", "server", "scenario"} for snapshot in snapshots)
        sleeps = [entry for entry in transcript if entry["request"]["op"] == "sleep"]
        assert len(sleeps) == 3 and all(entry["request"]["duration_ns"] == 100_000_000 for entry in sleeps)
        network = [f for f in faults if f["action"] in ("partition", "heal")]
        assert len(network) == 4
        before, after = network[2]["network_counters"], network[3]["network_counters"]
        final = world["network_counters"]
        links = ["client->server", "server->client"]
        assert any(after.get(k, {}).get("partition_dropped", 0) > before.get(k, {}).get("partition_dropped", 0) for k in links), "no traffic drops during partial healing"
        assert any(final.get(k, {}).get("delivered", 0) > after.get(k, {}).get("delivered", 0) for k in links), "no NIC acceptance after final healing"
    client = (run / "output" / "machines" / "client.console").read_text()
    server = (run / "output" / "machines" / "server.console").read_text()
    scenario = (run / "output" / "machines" / "scenario.console").read_text()
    results = RESULT.findall(client)
    assert len(results) == 1, "missing/ambiguous independent client result"
    status, acked, pre, post, epoch, missing = results[0]
    acked, pre, post, epoch, missing = map(int, (acked,pre,post,epoch,missing))
    assert status == expected.lower() and acked == 128
    assert pre == (32 if faulted else 128) and post == (32 if faulted else 128)
    phases = [tuple(map(int, row)) for row in PHASE.findall(client)]
    assert phases == ([(1,32,32),(2,32,64),(2,32,96),(3,32,128)] if faulted else
                      [(1,32,32),(1,32,64),(1,32,96),(1,32,128)]), phases
    probes = [(phase, json.loads(value)) for phase, value in PROBE.findall(scenario)]
    assert probes == ([(phase, {"attempts":4,"failed":4,"acked":0}) for phase in ("both","partial")] if faulted else []), probes
    scenario_results = re.findall(r"^SCENARIO_RESULT (.+)$", scenario, re.M)
    assert len(scenario_results) == 1, "missing/ambiguous controller check result"
    assert json.loads(scenario_results[0]) == dict(status=status, acked=acked, pre_restart=pre,
                                                 post_restart=post, generation=epoch, missing=missing)
    assert epoch == (3 if faulted else 1), "restart generations were not exercised"
    recovered = {int(g):int(n) for g,n,_ in READY.findall(server)}
    assert set(recovered) == (set([1,2,3]) if faulted else set([1]))
    missing_keys = MISSING.findall(client)
    assert len(missing_keys) == missing
    if expected == "FAIL":
        assert missing == 96 and all(int(g) < epoch for _,_,g in missing_keys), "failure did not identify lost restart-preceding acknowledgments"
        assert all(n == 0 for n in recovered.values()), "broken server unexpectedly recovered user data"
    else:
        assert missing == 0
        if faulted:
            assert recovered[2] == 32 and recovered[3] == 96, "healthy server did not reload acknowledged writes"
    if record.get("replay"):
        comparison = record["replay"]
        assert comparison["status"] == "MATCH" and comparison["fault_history_matches"] and comparison["witness_matches"]
    return {"outcome":expected,"faults":len(faults),"acked":acked,"pre_restart":pre,"post_restart":post,"generation":epoch,"missing":missing,"witness":report["witness"],"run":str(run)}

if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("run")
    p.add_argument("--expected", choices=["PASS", "FAIL"], default="PASS")
    p.add_argument("--no-faults", action="store_true")
    a = p.parse_args()
    print(json.dumps(verify(a.run, a.expected, not a.no_faults), indent=2))
