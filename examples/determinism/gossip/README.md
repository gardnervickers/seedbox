# gossip: sub-latency cross-machine traffic under parallel workers

**Mechanism probed.** The windowed scheduler's causal rounds and their output
ordering (the coordinator sorts frames by send time, machine and machine
sequence before routing), plus virtio-net TX pickup. Four machines each send a
UDP datagram to the other three every 200 us, below the 1 ms link latency, so
every round carries many frames from every machine. Each also keeps one TCP
stream to its ring neighbour (n1 -> n2 -> n3 -> n4 -> n1) with a 64-byte record
every 1 ms. If frame routing, TX pickup time or delivery order ever depends on
which worker thread finished first, a receive timestamp or the interleaving of
senders in some node's log changes.

**What each node does** (`gossip.py`, one thread, one selector). Binds UDP 9000
and TCP 9001, retries resolving and connecting to its neighbour every 10 ms (no
start ordering), logs every received record as `udp|tcp <sender> <seq>
<local_recv_monotonic_ns>`, appends new log lines to `/data/log` (its own named
volume) with fsync every 50 ms, runs 3 s, then prints and exits 0. A peer that
already finished is tolerated: UDP send errors are counted, a reset neighbour
stream is dropped and not reconnected.

**Output** (one block per service, n1 is the checked service):

```
node: n1
udp_received: N
tcp_received: N
udp_sent: N
udp_send_errors: N
tcp_sent: N
tcp_connect_attempts: N
tcp_out_state: up|gone|idle|connecting
first_20:
  udp n2 0 <recv_ns>
  ...
digest: <sha256 of all log lines>
file_digest: <sha256 of /data/log>
```

`digest` and `file_digest` should be equal (the file holds exactly the log).
The full log is ~45,000 lines per node, so only the first 20 are printed; the
digest covers every line and every receive timestamp.

**Comparing two runs.** Same seed (1103), fresh runs, compare all four
services' console bytes, or `seedbox diff RUN_A RUN_B` on the retained
directories. Any differing line is a D2 violation.

**Control knob: 1 versus 4 machine workers.** `seedbox run` always uses one
worker, so take a retained run's `output/worker-request.json`, copy it with
`machine_workers` set to 4 (and `machine_cpus` listing four host CPUs, `out`
pointing at a fresh directory, `host_control` null because the original run's
socket is gone), and run the retained engine on it directly:

```sh
seedbox run examples/determinism/gossip --full-state           # baseline, 1 worker
python3 - RUN/output/worker-request.json req4.json <<'PY'
import json, sys
r = json.load(open(sys.argv[1])); r["machine_workers"] = 4
r["machine_cpus"] = [4, 5, 6, 7]; r["out"] = "out4"; r["host_control"] = None
json.dump(r, open(sys.argv[2], "w"))
PY
build/current/seedbox __engine seedbox-internal-worker-v1 --request req4.json
```

Console bytes and witnesses must be identical for 1 and 4 workers, and for two
runs at 4 workers. Varying `--watchdog-ms` is a secondary negative control.
