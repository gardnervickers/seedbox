#!/usr/bin/env python3
"""Verify synthetic retained recovery evidence and actual acknowledged-key loss."""
import argparse
import json
from pathlib import Path
import re

RESULT = re.compile(r"^RESULT (pass|fail) acked=(\d+) pre_restart=(\d+) post_restart=(\d+) generation=(\d+) missing=(\d+)$", re.M)
READY = re.compile(r"^SERVER_READY generation=(\d+) recovered=(\d+) broken=(true|false)$", re.M)
MISSING = re.compile(r"^MISSING_ACK key=(\S+) expected=(\S+) acknowledged_generation=(\d+)$", re.M)

def verify(run, expected="PASS", faulted=True):
    run = Path(run)
    record = json.loads((run / "manifest.json").read_text())["record"]
    assert record["outcome"] == expected, (record["outcome"], record.get("error"))
    report = record["report"]
    assert report and report["reason"] == "COMPLETED", report
    assert report["workload_exit"] == {"service":"client", "code":0 if expected == "PASS" else 1}
    world = json.loads((run / "output" / "vmm.json").read_text())
    faults = world["faults"]
    assert world.get("fault_error") is None and not world.get("rejected_fault_effects")
    assert len(faults) == (8 if faulted else 0)
    for i, fault in enumerate(faults, 1):
        assert fault["id"] == i and fault["status"] == "applied", fault
        assert fault["applied_at_ns"] >= fault["requested_at_ns"], fault
    if faulted:
        network = [f for f in faults if f["action"] in ("partition", "heal")]
        assert len(network) == 4
        before, after = network[2]["network_counters"], network[3]["network_counters"]
        final = world["network_counters"]
        links = ["client->server", "server->client"]
        assert any(after.get(k, {}).get("partition_dropped", 0) > before.get(k, {}).get("partition_dropped", 0) for k in links), "no traffic drops during partial healing"
        assert any(final.get(k, {}).get("delivered", 0) > after.get(k, {}).get("delivered", 0) for k in links), "no NIC acceptance after final healing"
    client = (run / "output" / "machines" / "client.console").read_text()
    server = (run / "output" / "machines" / "server.console").read_text()
    results = RESULT.findall(client)
    assert len(results) == 1, "missing/ambiguous independent client result"
    status, acked, pre, post, epoch, missing = results[0]
    acked, pre, post, epoch, missing = map(int, (acked,pre,post,epoch,missing))
    assert status == expected.lower() and acked > 0 and pre > 0 and post > 0
    assert epoch == (3 if faulted else 1), "restart generations were not exercised"
    recovered = {int(g):int(n) for g,n,_ in READY.findall(server)}
    assert set(recovered) == (set([1,2,3]) if faulted else set([1]))
    missing_keys = MISSING.findall(client)
    assert len(missing_keys) == missing
    if expected == "FAIL":
        assert missing > 0 and all(int(g) < epoch for _,_,g in missing_keys), "failure did not identify lost restart-preceding acknowledgments"
        assert all(n == 0 for n in recovered.values()), "broken server unexpectedly recovered user data"
    else:
        assert missing == 0
        if faulted:
            assert recovered[2] > 0 and recovered[3] > 0, "healthy server did not reload acknowledged writes"
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
