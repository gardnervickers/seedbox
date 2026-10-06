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
causes FAIL; the checker does not plant a failure.

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
python3 sim/examples/faults/generate.py /absolute/generated-faults
```

Generation creates `fixed`, `broken`, and `broken-no-faults`, copies the canonical
SDK from `sim/python/seedbox` into each build context, and refuses to overwrite
existing projects. Every image uses `docker.io/library/python:3.13-alpine` with
only the Python standard library. The source `compose.yaml` is a template whose
build context acquires the SDK during generation. The Python controller is in
`workload/scenario.py`; the protocol used by the independent client is separate
from the fault RPC protocol.

Run all cases and effect-error controls from `nix develop ./sim` on the qualified
Linux host:

```sh
bash sim/seedbox/tests/accept-runtime.sh /absolute/bzImage 14 /absolute/fault-evidence faults
```

The evidence output must be fresh. The harness writes its report and command logs
incrementally, preserving completed evidence if a later case fails. The
[acceptance record](ACCEPTANCE.md) contains the validated runtime inputs, all nine
outcomes, matching replay witnesses, and review results.

To invoke the harness directly after building the CLI, VMM, and guest supervisor:

```sh
python3 sim/examples/faults/run.py \
  --seedbox "$PWD/sim/seedbox/target/release/seedbox" \
  --vmm "$PWD/sim/vmm/target/release/seedbox-vmm" \
  --kernel /absolute/bzImage --guest-init /absolute/guest-init \
  --cpu 14 --state /absolute/fault-state --output /absolute/fault-evidence
```

The harness checks fixed PASS, broken FAIL, broken-without-faults PASS, and replay
of all three. The healthy case also replays with margin 131072 and undo 16;
ordinary replay disables undo. It uses 512 MiB per guest to accommodate the Python
image, 2 ms stop tolerance, and margin 98304;
`--margin` overrides the recorded margin. Runs are not silently retried. Every
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

Portable tests require no KVM. Run `python3 sim/examples/faults/portable.py` to
launch the actual Python server and client processes, abruptly stop and restart
the server twice, and verify healthy recovery, broken loss, and the no-restart
control through independent TCP requests. These checks establish workload
behavior; runtime fault delivery and deterministic replay need the Linux suite.
