# Python TCP faults and recovery

This example has three Python services: a key/value server, an independent client,
and a dedicated scenario controller. The server and client use real TCP. The
controller uses the public `seedbox.World` API through the guest supervisor's Unix
socket at `/run/seedbox/control.sock`. `x-seedbox.control.service` selects the
controller, and `x-seedbox.check.service` selects its exit status as the result.
The controller can reach all services and is immune to stop, restart, and
partition operations. The simulation host does not issue workload fault commands.

The healthy server saves and syncs each value before acknowledging its PUT, then
reloads complete records after process restart. The deliberately broken server
keeps values only in memory. Both save a startup generation counter for diagnostics.
The client records only actual PUT acknowledgements and checks every recorded
value with GET after the scenario. Each missing acknowledged value is printed and
causes FAIL; the checker does not plant a failure. A timed-out PUT retains its
key/value until a complete reply with the expected generation is received. Only
then does the client advance its sequence and record one acknowledgement. LOAD
has one ten-second deadline covering successful progress, retries and blocking
I/O; its control response has a shared twelve-second transport bound. Neither
the scenario nor the portable test retries an entire LOAD after a lost reply.

The scenario asks the client's small TCP control API to complete batches of 32
acknowledged writes. Once a batch completes, it decides the next operation from
that observed progress. The eight fault operations are stop, restart, partition,
partition, heal, heal, stop, restart. Calls block until their effects are confirmed;
`world.sleep(0.1)` demonstrates waits in simulated time. The client checks real
application readiness after each restart. Two groups of four failed write attempts
demonstrate dropped traffic under both overlapping cuts and after the first heal.
Another acknowledged batch demonstrates delivery after the final heal.
`world.inspect()` confirms the independent handles at each stage.

The scenario completes 128 actual acknowledgements across generations 1, 2, and 3
and exits according to the independent client's result. The healthy case must
recover 32 records on its second start and 96 on its third. The broken case loses
96 acknowledged values. A broken-server control runs the same 128 acknowledgements
without faults and must PASS, demonstrating that the failure depends on restart.

Generate the self-contained projects before using the Compose files:

```sh
python3 examples/faults/generate.py /absolute/generated-faults
```

Generation creates `fixed`, `broken`, and `broken-no-faults`, copies the canonical
SDK from `python/seedbox` into each build context, and refuses to overwrite
existing projects. Every image uses `docker.io/library/python:3.13-alpine` with
only the Python standard library. The source `compose.yaml` is a template whose
build context acquires the SDK during generation. The Python controller is in
`workload/scenario.py`; the protocol used by the independent client is separate
from the fault RPC protocol.

Run all cases and effect-error controls from `nix develop` on the qualified
Linux host:

```sh
bash crates/seedbox/tests/accept-runtime.sh /absolute/bzImage 14 /absolute/fault-evidence faults
```

The evidence output must be fresh. The harness writes its report and command logs
incrementally, preserving completed evidence if a later case fails. The
[acceptance record](ACCEPTANCE.md) identifies the qualified binaries, retained
outcomes, replay witnesses and review results. Each engine revision needs its
own live acceptance; older records remain archived separately.

To invoke the harness directly after building the CLI, VMM, and guest supervisor:

```sh
python3 examples/faults/run.py \
  --seedbox "$PWD/target/release/seedbox" \
  --vmm "$PWD/crates/seedbox-vmm/target/release/seedbox-vmm" \
  --kernel /absolute/bzImage --guest-init /absolute/guest-init \
  --cpu 14 --state /absolute/fault-state --output /absolute/fault-evidence
```

The harness checks fixed PASS, broken FAIL, broken-without-faults PASS, and replay
of all three. Replays use the default performance settings; only the
lifecycle case varies them (below). It uses 512 MiB per guest to accommodate the
Python image and 2 ms stop tolerance. Runs are not silently retried. Every
run and replay checks restoration of the workaround MSR on every CPU sibling,
retaining its before and after values.

Raw `output/vmm.json` retains the fault effects, control request/response history,
simulated timing, and network counters. Counters must show continuing drops after
partial healing and renewed NIC acceptance after final healing. Already routed
frames remain queued; new sends crossing a cut are dropped. NIC acceptance includes
its bounded backlog, independently of IRQ notification. Receiver drops cannot
count as delivery. `verify.py RUN --expected PASS` checks the retained evidence;
use `--expected FAIL` for the broken case and `--no-faults` for its control.

For missing effects, compile the guest supervisor with `-DSEEDBOX_DROP_EFFECTS`
and add `--negative-guest-init /absolute/no-effects`. For explicit rejection,
compile it with `-DSEEDBOX_REJECT_EFFECTS` and add
`--rejecting-guest-init /absolute/reject-effects`. Both must produce ERROR. The
controls perform their port I/O inside the guests.

Add `--delayed-guest-init /absolute/delayed-ready` to run four more executions
after the existing nine-case matrix. Compile that supervisor with
`-DSEEDBOX_TEST_READY_DELAY_NS=4000000000ULL`. The focused projects retain
`SEEDBOX_TEST_DELAY_READY=1` on the server only, so its initial application starts
normally but its supervisor announces management readiness four simulated seconds
later. The controller and client retain ordinary startup.

The early-request case records 32 generation-1 acknowledgements, requests a stop,
and checks that its admission precedes the server's management readiness. The stop
must take effect after readiness and the fixed dispatch delay. A restart and 96
more acknowledgements then verify all 128 saved values. Its replay must MATCH.

The cutoff case requests a stop immediately under a three-second simulation limit.
It must retain the pending call without a response or applied effect and end as
TIMEOUT, before the original admission-plus-five-second acknowledgement deadline.
Its replay must MATCH. Reaching the real acknowledgement deadline remains ERROR;
a simulation cutoff does not manufacture an acknowledgement failure. The verifier
rejects a run whose request was too late to exercise admission before the cutoff.
Use `verify.py RUN --focused early-request` or
`verify.py RUN --focused readiness-cutoff` to inspect these records separately.

The control-cases scenario runs after the matrix and is replayed once. It rejects
restart on a running server and a second stop, closes its first connection, then
reconnects to restart the server. The target confirms that it has no fault socket.
After deliberately exiting with code 17, it is reported as `Exited(17)` and
stop/restart return `target_unavailable`. The controller completes another time
query and classifies that workload exit as FAIL. The verifier checks both sessions,
all four recoverable errors and the absence of an infrastructure ERROR.
Use `verify.py RUN --focused control-cases` to inspect this evidence separately.

The lifecycle-cases scenario runs next. It is replayed with the default settings
and again with `--undo 0 --margin 16384` (the margin is read only with undo off);
both must MATCH. It pauses the
whole server machine while an established connection sends a request: nothing
answers until resume, and then the held request is answered on the same
connection. It crashes the server with another connection open: the peer sees
neither FIN nor RST, only silence. After reboot the server reports a new kernel
`boot_id` and has lost a marker file it wrote to RAM before the crash. The
scenario also checks six `invalid_state` transitions, stop of a down machine
returning `target_unavailable`, and `inspect()` reporting `Paused` and `Down`.
The verifier checks the applied effect times, the crashed incarnation record and
the second boot in the server console. With all optional supervisors supplied,
the complete matrix contains seventeen executions. Use
`verify.py RUN --focused lifecycle-cases` to inspect this evidence separately.

Portable tests require no KVM. Run `python3 examples/faults/portable.py` to
launch the actual Python server and client processes, abruptly stop and restart
the server twice, and verify healthy recovery, broken loss, and the no-restart
control through independent TCP requests. It also persists a write before losing
its reply, rejects invalid acknowledgements without changing the ledger, and
checks successful-progress, unanswered-response and byte-trickle expiry. Recovery
counts remain exactly 32 and 96; the broken restart case must lose exactly 96 of
128 acknowledged records. These checks establish workload
behavior; runtime fault delivery and deterministic replay need the Linux suite.
