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

This example and its full acceptance matrix use `batch=32`.

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

Run the generated projects through the current public CLI on a Linux KVM host:

```sh
build/current/seedbox run /absolute/generated-faults/fixed
build/current/seedbox run /absolute/generated-faults/broken
build/current/seedbox run /absolute/generated-faults/broken-no-faults
build/current/seedbox replay SIMULATION_ID
```

Expected oracle results are PASS, FAIL and PASS respectively; replay of each
completed simulation must MATCH. The guest owns its assertions. Use
`seedbox logs ID [MACHINE]` and `seedbox events ID` while execution is active;
these streams have no historical backlog. Server startup owns runtime inputs.
The previous host runner and archived-console acceptance workflow have been
removed. `verify.py` remains an offline reader for historical retained evidence.
Generation helpers retain the control, lifecycle, early-request and cutoff
scenario sources for focused guest checks. The default synthetic merge checks
run through `bash harness/check.sh linux-apps`.

### Historical focused scenario evidence

The following descriptions and offline verification commands describe the
older retained-console acceptance records, rather than the current live server
workflow. New simulations use exact stop boundaries; the former 2 ms tolerance
changed those worlds' results.

The control-cases scenario runs after the matrix and is replayed once. It rejects
restart on a running server and a second stop, closes its first connection, then
reconnects to restart the server. The target confirms that it has no fault socket.
After deliberately exiting with code 17, it is reported as `Exited(17)` and
stop/restart return `target_unavailable`. The controller completes another time
query and classifies that workload exit as FAIL. The verifier checks both sessions,
all four recoverable errors and the absence of an infrastructure ERROR.
Use `verify.py RUN --focused control-cases` to inspect this evidence separately.

The lifecycle-cases scenario runs next. It is replayed with the default settings
and again with a different host watchdog interval; both were required to MATCH. It first pauses only the server's process (SIGSTOP) with one
connection established and another opened while it is frozen: both requests
hang with no FIN or RST, and both are answered after `resume_process`. It then
pauses the whole server machine while an established connection sends a request: nothing
answers until resume, and then the held request is answered on the same
connection. It crashes the server with another connection open: the peer sees
neither FIN nor RST, only silence. After reboot the server reports a new kernel
`boot_id` and has lost a marker file it wrote to RAM before the crash. The
scenario also checks seven `invalid_state` transitions, stop of a down machine
returning `target_unavailable`, and `inspect()` reporting `Frozen`, `Paused` and
`Down`.
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
