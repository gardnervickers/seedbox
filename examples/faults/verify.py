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

def shared_kernel(world):
    return {machine["name"] for machine in world["machines"]} == {"kernel"}


def machine_for(world, service):
    return "kernel" if shared_kernel(world) else service


def expected_machines(world):
    return {"kernel"} if shared_kernel(world) else {"client", "server", "scenario"}


def console(run, service):
    directory = Path(run) / "output/machines"
    separate = directory / (service + ".console")
    if separate.exists():
        return separate.read_text()
    prefix = service + " | "
    return "".join(line[len(prefix):] for line in (directory / "kernel.console").read_text().splitlines(keepends=True)
                   if line.startswith(prefix))


def verify_transport(world):
    channels = {machine["name"]: machine["control_channel"] for machine in world["machines"]}
    assert all(channel["transport"] == "virtio-console-v1" for channel in channels.values()), channels
    assert channels[machine_for(world, "scenario")]["tx_bytes"] > 0, "controller did not submit shared-memory bytes"
    if any(entry.get("delivered_at_ns") is not None for entry in world["control_history"]):
        assert channels[machine_for(world, "scenario")]["rx_dma_bytes"] > 0, "controller replies did not use shared-memory DMA"

def verify(run, expected="PASS", faulted=True):
    run = Path(run)
    record = json.loads((run / "manifest.json").read_text())["record"]
    assert record["outcome"] == expected, (record["outcome"], record.get("error"))
    report = record["report"]
    assert report and report["reason"] == "COMPLETED", report
    assert report["workload_exit"] == {"service":"scenario", "code":0 if expected == "PASS" else 1}
    world = json.loads((run / "output" / "vmm.json").read_text())
    verify_transport(world)
    faults = world["faults"]
    assert {machine["name"] for machine in world["machines"]} == expected_machines(world)
    assert world.get("fault_error") is None and not world.get("rejected_fault_effects")
    assert len(faults) == (8 if faulted else 0)
    assert [f["action"] for f in faults] == (OPERATIONS if faulted else [])
    for i, fault in enumerate(faults, 1):
        assert fault["id"] == i and fault["status"] == "applied", fault
        assert fault["applied_at_ns"] >= fault["requested_at_ns"], fault
        if fault["action"] in ("stop", "restart"):
            assert fault["service"] == "server", fault
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
        if not shared_kernel(world):
            before, after = network[2]["network_counters"], network[3]["network_counters"]
            final = world["network_counters"]
            links = ["client->server", "server->client"]
            assert any(after.get(k, {}).get("partition_dropped", 0) > before.get(k, {}).get("partition_dropped", 0) for k in links), "no traffic drops during partial healing"
            assert any(final.get(k, {}).get("delivered", 0) > after.get(k, {}).get("delivered", 0) for k in links), "no NIC acceptance after final healing"
    client = console(run, "client")
    server = console(run, "server")
    scenario = console(run, "scenario")
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

def focused_evidence(run, outcome, reason):
    run = Path(run)
    record = json.loads((run / "manifest.json").read_text())["record"]
    assert record["outcome"] == outcome, (record["outcome"], record.get("error"))
    report = record["report"]
    assert report and report["reason"] == reason and report["witness"], report
    world = json.loads((run / "output" / "vmm.json").read_text())
    verify_transport(world)
    assert {machine["name"] for machine in world["machines"]} == expected_machines(world)
    assert world.get("fault_error") is None and not world.get("rejected_fault_effects"), world
    assert world["control_latency_ns"] > 0
    if record.get("replay"):
        replay = record["replay"]
        assert replay["status"] == "MATCH" and replay["fault_history_matches"] and replay["witness_matches"], replay
    return run, record, report, world


def verify_early_request(run):
    run, record, report, world = focused_evidence(run, "PASS", "COMPLETED")
    assert report["workload_exit"] == {"service": "scenario", "code": 0}
    faults = world["faults"]
    assert [fault["action"] for fault in faults] == ["stop", "restart"], faults
    for number, fault in enumerate(faults, 1):
        assert fault["id"] == number and fault["service"] == "server" and fault["status"] == "applied", fault
        assert fault["requested_at_ns"] <= fault["applied_at_ns"] < fault["request"]["deadline_ns"], fault
    readiness = world["management_readiness"]
    assert len(readiness) == len(expected_machines(world)) and {row["machine"] for row in readiness} == expected_machines(world), readiness
    ready = next(row["at_ns"] for row in readiness if row["machine"] == machine_for(world, "server"))
    stop = faults[0]
    assert stop["requested_at_ns"] < ready, "stop was not requested before target supervisor readiness"
    assert stop["applied_at_ns"] >= ready + world["control_latency_ns"], "stop did not wait for readiness and dispatch latency"
    transcript = world["control_history"]
    assert [entry["request"]["op"] for entry in transcript] == ["stop", "sleep", "restart"], transcript
    for number, entry in enumerate(transcript, 1):
        request, response = entry["request"], entry["response"]
        assert entry["relay_id"] == number and request["id"] == number and response["id"] == number and response["ok"], entry
        assert response["requested_at_ns"] + world["control_latency_ns"] == response["admitted_at_ns"], entry
        assert response["effect_at_ns"] + world["control_latency_ns"] == response["replied_at_ns"] == entry["delivered_at_ns"], entry
    assert transcript[0]["response"]["effect_at_ns"] == stop["applied_at_ns"]
    assert transcript[2]["response"]["effect_at_ns"] == faults[1]["applied_at_ns"]
    client = console(run, "client")
    server = console(run, "server")
    scenario = console(run, "scenario")
    assert RESULT.findall(client) == [("pass", "128", "32", "96", "2", "0")], client
    assert [tuple(map(int, phase)) for phase in PHASE.findall(client)] == [(1,32,32),(2,32,64),(2,32,96),(2,32,128)], client
    assert not MISSING.findall(client)
    assert READY.findall(server) == [("1", "0", "false"), ("2", "32", "false")], server
    results = re.findall(r"^EARLY_REQUEST_RESULT (.+)$", scenario, re.M)
    assert len(results) == 1 and json.loads(results[0]) == dict(status="pass", acked=128,
            pre_restart=32, post_restart=96, generation=2, missing=0), scenario
    return dict(case="early_request", outcome="PASS", faults=2, acked=128, missing=0,
                requested_at_ns=stop["requested_at_ns"], ready_at_ns=ready,
                applied_at_ns=stop["applied_at_ns"], witness=report["witness"], run=str(run))


def verify_cutoff(run):
    run, record, report, world = focused_evidence(run, "TIMEOUT", "SIMULATED_TIME_LIMIT")
    assert report["workload_exit"] is None and not report["faults"], report
    assert world["outcome"] == "duration"
    transcript = world["control_history"]
    assert len(transcript) == 1, transcript
    entry = transcript[0]
    assert entry["relay_id"] == 1 and entry["request"] == dict(version=1, id=1, op="stop", target={"service":"server"}), entry
    assert entry["status"] == "cut_off" and entry.get("response") is None, entry
    cutoff = entry["cut_off_at_ns"]
    assert cutoff == 3_000_000_000 and entry["admitted_done"] is True
    assert entry["requested_at_ns"] + world["control_latency_ns"] == entry["admitted_at_ns"] < cutoff, entry
    assert entry["effect_at_ns"] is None and entry["replied_at_ns"] is None
    assert entry["error"] is None and entry["fatal"] is False and entry["fault_id"] == 1, entry
    faults = world["faults"]
    assert len(faults) == 1, faults
    fault = faults[0]
    assert fault["id"] == 1 and fault["action"] == "stop" and fault["service"] == "server", fault
    assert fault["status"] == "requested" and fault["applied_at_ns"] is None, fault
    assert entry["plan"] == fault["request"] and fault["requested_at_ns"] == entry["admitted_at_ns"], (entry, fault)
    assert cutoff < fault["request"]["deadline_ns"] == entry["admitted_at_ns"] + 5_000_000_000
    assert all(row["machine"] != machine_for(world, "server") for row in world["management_readiness"]), "target became ready before cutoff"
    if not shared_kernel(world):
        assert any(row["machine"] == "scenario" for row in world["management_readiness"])
    client = console(run, "client")
    scenario = console(run, "scenario")
    assert not RESULT.findall(client) and "RESULT " not in scenario, "pending stop produced a completed workload result"
    return dict(case="readiness_cutoff", outcome="TIMEOUT", faults_applied=0,
                requested_at_ns=entry["requested_at_ns"], admitted_at_ns=entry["admitted_at_ns"],
                cut_off_at_ns=cutoff, deadline_ns=fault["request"]["deadline_ns"],
                witness=report["witness"], run=str(run))


def verify_control_cases(run):
    run, record, report, world = focused_evidence(run, "FAIL", "COMPLETED")
    assert report["workload_exit"] == {"service":"scenario", "code":1}
    if shared_kernel(world):
        assert any(row["service"] == "server" and row["status"] == 17 for row in world["service_exits"])
    else:
        assert next(machine["guest_exit_code"] for machine in world["machines"] if machine["name"] == "server") == 17
    assert [fault["action"] for fault in world["faults"]] == (["stop", "restart", "restart"] if shared_kernel(world) else ["stop", "restart"])
    assert all(fault["status"] == "applied" for fault in world["faults"])
    transcript = world["control_history"]
    assert {entry["session_id"] for entry in transcript} == {1, 2}
    ids = {}
    for relay, entry in enumerate(transcript, 1):
        session = entry["session_id"]
        ids[session] = ids.get(session, 0) + 1
        assert entry["relay_id"] == relay and entry["request"]["id"] == ids[session]
        assert entry["response"]["id"] == ids[session]
        assert entry["delivered_at_ns"] == entry["response"]["replied_at_ns"]
    errors = [entry["response"]["error"]["code"] for entry in transcript if not entry["response"]["ok"]]
    assert errors == (["invalid_state"] * 3 if shared_kernel(world) else
                      ["invalid_state", "invalid_state", "target_unavailable", "target_unavailable"]), errors
    assert transcript[-1]["request"]["op"] == "now" and transcript[-1]["response"]["ok"]
    server = console(run, "server")
    assert server.count("TARGET_SOCKET absent") == (3 if shared_kernel(world) else 2), server
    generations = [1, 2, 3] if shared_kernel(world) else [1, 2]
    assert re.findall(r"^TARGET_IO_PORTS denied generation=(\d+)$", server, re.M) == list(map(str, generations)), server
    scenario = console(run, "scenario")
    results = re.findall(r"^CONTROL_CASES_RESULT (.+)$", scenario, re.M)
    assert len(results) == 1 and json.loads(results[0]) == {"errors":errors,"target_exit":17,"reconnected":True}, scenario
    return dict(case="control_cases", outcome="FAIL", errors=errors, target_exit=17,
                witness=report["witness"], run=str(run))


def verify_inherited_io(run):
    run, record, report, world = focused_evidence(run, "FAIL", "COMPLETED")
    assert report["workload_exit"] == {"service": "scenario", "code": 1}
    assert [fault["action"] for fault in world["faults"]] == ["stop", "restart"]
    assert all(fault["status"] == "applied" for fault in world["faults"])
    server = console(run, "server")
    assert re.findall(r"^TARGET_IO_PROBE generation=(\d+)$", server, re.M) == ["1", "2"], server
    assert re.findall(r"^TARGET_IO_PORTS denied generation=(\d+)$", server, re.M) == ["1"], server
    assert "application inherited supervisor I/O permissions" in server, server
    return dict(case="inherited_io", outcome="FAIL", detected_generation=2,
                witness=report["witness"], run=str(run))


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("run")
    p.add_argument("--expected", choices=["PASS", "FAIL"], default="PASS")
    p.add_argument("--no-faults", action="store_true")
    p.add_argument("--focused", choices=["early-request", "readiness-cutoff", "control-cases"])
    a = p.parse_args()
    verifier = {"early-request": verify_early_request, "readiness-cutoff": verify_cutoff, "control-cases": verify_control_cases}.get(a.focused)
    print(json.dumps(verifier(a.run) if verifier else verify(a.run, a.expected, not a.no_faults), indent=2))
