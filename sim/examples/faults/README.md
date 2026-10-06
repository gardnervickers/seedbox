# Synthetic TCP faults and recovery

The dependency-free Rust fixture has a small key/value server and an independent
client. The healthy server appends and syncs each value before returning its PUT
acknowledgment, then reloads complete records after process restart. The deliberately
broken server keeps user values only in memory. The same client records actual
acknowledgments and GET-checks every value after the workload; it plants no failure.
Missing acknowledged keys are printed individually and produce FAIL.

Both servers save a startup generation counter solely as exposure evidence. The
client requires acknowledged writes from generation 1 before the first restart and
from generation 3 after both restarts. The healthy server must also report recovered
records on both starts. Empty workloads and unexercised restarts cannot pass.

`generate.py DESTINATION` creates self-contained fixed, broken, and
broken-no-faults projects, with the same client source and deterministic seed 1009.
The fixed/broken schedule stops at 5s/restarts at 6s, partitions at 8s and 9s, heals the first
handle at 10s and the last at 11s, then stops at 12s/restarts at 13s. The client writes for 15s and
checks recovery afterward. The shorter broken-no-faults control must PASS, proving
that the state-loss failure depends on restarting the server.

Run all cases, including both acknowledgment-error controls, from
`nix develop ./sim` on the qualified Linux host:

```sh
bash sim/seedbox/tests/accept-runtime.sh /absolute/bzImage 14 /absolute/fault-evidence faults
```

The output directory must be fresh. The harness writes its report and command logs
incrementally, preserving completed evidence if a later case fails. See the
[acceptance record](ACCEPTANCE.md) for the validated inputs and results.

To run the three workload cases directly, build the CLI/VMM and
`sim/guest/build-compose.sh /absolute/guest-init`, then run with access to the
existing CPU lease and MSR devices:

```sh
python3 sim/examples/faults/run.py \
  --seedbox "$PWD/sim/seedbox/target/release/seedbox" \
  --vmm "$PWD/sim/vmm/target/release/seedbox-vmm" \
  --kernel /absolute/bzImage --guest-init /absolute/guest-init \
  --cpu 14 --state /absolute/fault-state --output /absolute/fault-evidence
```

The harness checks fixed PASS, broken FAIL, broken-without-faults PASS, and retained
replay for all three. Replay disables undo as a performance comparison. Default
2ms stop tolerance stays enabled while faults land exactly. The fixture uses
256MiB per guest and explicit margin 98304; `--margin` overrides that recorded
performance setting. Runs are not silently retried. Every run/replay asserts the
workaround MSR is restored on all CPU siblings and records before/after values.

Traffic counters must show continuing drops after partial healing and renewed
NIC acceptance after the final handle heals. Already routed frames remain queued;
new sends crossing a cut are dropped. NIC acceptance includes its bounded backlog,
independently of IRQ notification; unready/full-backlog receiver drops cannot count
as delivery. Raw histories and phase counters remain in `output/vmm.json`.

For the missing-effect control, compile the supervisor with
`$SEEDBOX_STATIC_CC -O2 -static -DSEEDBOX_DROP_EFFECTS -o /absolute/no-effects sim/guest/compose-init.c`
and add `--negative-guest-init /absolute/no-effects`. For explicit rejection,
compile with `-DSEEDBOX_REJECT_EFFECTS` and add
`--rejecting-guest-init /absolute/reject-effects`. Both must produce ERROR; the
latter executes the real 32-bit guest failure-port write. The ordinary fixed run
exercises the success port. Neither control runs privileged port I/O on the host.

`verify.py RUN --expected PASS` checks retained synthetic evidence separately;
use `--no-faults` for the memory-only control. Native tests compile and run the
actual binary, kill/restart real server processes, verify durable versus lost
keys, and run the independent client against healthy, broken, and no-restart
servers. The existing broader acceptance suite remains separate and unchanged.
