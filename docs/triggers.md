# Committed console triggers (development engine)

Issue #62 adds a deliberately small event surface: actual COM1 UART transmit
bytes (including debug port 0xe9) on one prepared machine. Application stdout
reaches this stream through the ordinary supervisor. Markers are opaque bytes;
no log JSON, protocol decoding, packet counts, host log tailing or application
commit inference is involved. COM2 and virtio management traffic are excluded.
Network/control event selectors and connection-selected triggers are deferred.
This changes the engine. Use `--allow-dev-engine`; the release policy and kernel
pins have not changed. These controls do not qualify this development engine.

`World.arm_console_stop(source, marker, target, match_timeout=seconds)` and
`World.arm_console_partition(source, marker, left, right, match_timeout=seconds)`
return a `Trigger`. Source and targets are service names or `Machine` objects.
Partition triggers also return `trigger.partition`, a handle usable with
`world.heal()` after the effect. Arm confirmation means armed, not that a fault
has happened. `world.cancel_trigger(trigger)` cancels only an unmatched trigger.
An expired, cancelled, matched or fired trigger returns a correlated error.
There are no implicit retries or heals. The existing `inspect()` includes a
`triggers` array only after a successful arm. `fired` means the effect was
submitted to the fault engine; process termination still requires its ordinary
reap acknowledgement. Inspect the machine state for confirmation.
At effect time, the action goes through the ordinary fault admission check.
If it fails, the trigger becomes `rejected`, retains `rejection_reason`, and
submits no fault or fault ID. For example, a stop trigger is rejected if control
has already stopped its target before the trigger's effect time.

## Time and commitment contract

1. Admission is `requested_at_ns + control_latency_ns`, exactly as for existing
   RPCs. Arm and cancel take effect at admission, with the existing delayed
   reply. No console history is searched. Bytes before admission cannot supply
   even part of a prefix. New bytes at admission can match after the barrier.
2. Console observation happens after the VMM exit judge accepts a deterministic
   exit, and after the real UART sink transmits. A discarded/rolled-back exit
   never enters that sink or the matcher. DLAB register writes do not transmit.
   Console ports have zero charged-I/O cost. Every committed console byte stops
   the armed source's slice immediately, even when it does not match the marker,
   before another candidate can be observed. With a stop tolerance, these extra
   slice boundaries change scheduling even if the trigger never fires. Arming
   and the resulting trigger state are recorded in the transcript, so replay
   reproduces the same scheduling and is unaffected. The
   accepted device effect and pending KVM response are irrevocable; undo cannot
   cross this exit. Completing KVM's held I/O later does not emit the byte again.
   Timer-port I/O is not an observation surface: a peer's already-sampled
   response and remaining charge survive a trigger boundary unchanged.
3. Match time is the committed time of the final byte. Each trigger matches
   once, in `[armed_at_ns, armed_at_ns + match_timeout_ns)`. The timeout is
   positive and checked for overflow. A marker at the exclusive deadline is
   too late. Expiration removes the matcher without creating a fault or an
   infrastructure error. A missing marker is visible in retained state; the
   scenario owns whether that makes its check fail.
4. Effect time is `matched_at_ns + control_latency_ns`. This fixed semantic
   delay is at least the world lookahead plus its early-delivery allowance.
   The match cannot insert an effect behind another machine's committed time.
   Every active machine catches up to the exact effect barrier before the
   fault is applied, without running beyond it. Halted/inactive machines remain
   at their deterministic held state. No lagging peer's past changes.
5. At a shared barrier, control admission/reply runs first, then trigger
   expiration/effect submission, then ordinary fault processing, then machine
   execution/delivery. Same-time trigger effects follow ascending arm ID,
   regardless of source-machine tie order. Ordinary faults share sequential
   IDs assigned at submission; their existing fault ordering remains intact.
   A same-time cancellation wins over expiry, but cannot cancel an earlier
   committed match. Matching duplicates cannot submit another effect.
6. A partition affects new routing decisions only. Already-routed response
   frames remain in flight. A process stop has the existing five simulated
   second effect/acknowledgement deadline after submission. Missing/rejected
   acknowledgements for submitted effects fail through the ordinary fault
   machinery. Admission rejection creates only a rejected trigger record. At run
   cutoff, unmatched and matched-but-not-submitted triggers retain `cut_off_at_ns`
   and their partial or scheduled state. A source can be ahead of an exiting controller;
   a committed match later than that terminal clock is retained as cut off,
   without submitting an effect. Submitted effects remain in the fault transcript,
   including an outstanding process effect when execution ends.

## Bounds and retained evidence

There are at most 64 successfully armed triggers over the entire run, including
expired/cancelled ones. A marker contains 1..=256 bytes. A streaming KMP matcher
keeps a prefix counter and a derived failure table per trigger; it never buffers
an unbounded console suffix. The source-to-world handoff holds one committed
byte. A machine crash resets that source's unmatched prefixes so markers cannot
span incarnations. Process stop/restart leaves the machine's console stream
intact. Checked byte counts and incarnation reset counts are retained with the prefix.

Snapshots copy matcher tables, records, pending effects, faults and control
state when triggers are present. The terminal `vmm.json` has a `triggers` array
and the world witness hashes its canonical JSON under the domain
`seedbox-committed-triggers-v1`. The host validates source/action resolution,
arm/cancel correlation, marker bounds, timestamps, status, effect latency,
fault IDs and effect deadlines. The engine remains the authority for actual
commitment; structural host validation is not independent application evidence.

When no trigger has ever been armed, the matcher has no boundary, machines do
not add console stops, no trigger key is emitted by inspect/report, and no
trigger bytes enter the witness. Existing results and witness inputs retain
their exact representation. Engine identity and existing host performance
measurements are naturally separate from deterministic result bytes.

## AMD KVM validation handoff

None of these KVM controls have run on the development Mac. On the AMD Zen KVM
host, enter the project's existing development shell, use the already accepted
kernel (do not rebuild/change pins), reserve a CPU with its SMT sibling idle,
and use fresh output/state paths. Set `KERNEL` to the absolute bzImage path,
`CPU` to that reserved logical CPU, and `OUT` to a fresh absolute directory.

Build and run the ordinary checks:

```sh
cargo build --release -p seedbox --locked
cargo build-vmm
cargo test-vmm
cargo fmt -p seedbox --check
cargo clippy -p seedbox --all-targets --locked -- -D warnings
cargo test --workspace --locked
bash harness/acceptance.sh portable
```

For the ignored KVM controls, build the test binary with the project alias:

```sh
cargo test-vmm --no-run --message-format=json > /tmp/seedbox-trigger-tests.jsonl
TEST_BINARY=$(python3 -c 'import json; print(next(x["executable"] for x in map(json.loads,open("/tmp/seedbox-trigger-tests.jsonl")) if x.get("reason")=="compiler-artifact" and x.get("profile",{}).get("test") and x.get("executable")))')
```

Run the focused test wrapper. It acquires the existing CPU/SMT locks, saves,
sets and verifies the Zen MSR workaround, directly owns/reaps each test process,
and restores/verifies the original MSRs before releasing the lease:

```sh
sudo -n env PATH="$PATH" python3 harness/test-triggers.py "$CPU" "$TEST_BINARY"
```

Expected: all five `committed_trigger` World tests pass (reversed machine ties,
undo 0/16/256, a lagging peer with unpaid charged I/O, retention of an already
routed response, snapshot restore, old console exclusion, and unused report/witness byte equality).
The forced-discard test must show one real rollback, exactly one committed
console byte, and no byte on subsequent completion. It deliberately forces the
actual KVM rollback branch with a test timebase; this does not establish PMU
skid recovery rates or engine qualification.

Run the guest-owned fixture through the public CLI:

```sh
mkdir -p "$OUT"
sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" target/release/seedbox run harness/acceptance/workloads/triggers.yaml --kernel "$KERNEL" --cpu "$CPU" --state "$OUT/state" --allow-dev-engine --full-state > "$OUT/run.log"
RUN=$(sed -n 's/^[A-Z]*: //p' "$OUT/run.log")
sudo -n env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu "$CPU" --allow-dev-engine
sudo -n env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu "$CPU" --allow-dev-engine --undo 0
sudo -n env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu "$CPU" --allow-dev-engine --undo 16
sudo -n env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu "$CPU" --allow-dev-engine --margin 32768
```

Expected: initial PASS/exit 0, every replay MATCH/exit 0. The scenario console
ends with `committed console trigger controls passed`. `vmm.json` retains five
triggers: cancelled old marker, exactly one fired partition, expired missing
marker, fired stop, and unmatched cutoff. Its fault transcript contains one
partition, its heal, the committed stop and the explicit restart; dropped peer
traffic is positive and post-heal traffic succeeds. This fixture uses a plain
synthetic marker and an explicitly earlier completed response. The World test
above supplies the stronger already-routed-but-still-in-flight control.

Also repeat the selected retained engine world/input closure with `--digest`
and with `--digest --trace`; compare complete world/machine witnesses, state
digests, console bytes, trigger records and network/fault effects. Repeat the
same seed/input closure rather than rebuilding registry tags. To check unused
behavior against the parent engine, build the parent revision in a separate
checkout with `cargo build-vmm`, then run the same retained no-trigger world
JSON and seed on each binary using `harness/batch.sh` and its scoped lease/MSR
handling. Compare every deterministic result, console and witness byte; exclude
only the existing host/PMU performance measurements. Replay across changed
engine identities is intentionally rejected, so this parent comparison uses
low-level retained world inputs rather than claiming cross-engine replay.

A separately budgeted changed-engine AMD qualification is still required.
There is no MinIO accepted-PUT trigger here. That needs independent application
commit evidence and a measurement showing the marker's safe effect precedes
routing the selected return bytes; a generic log line cannot establish it.

For concrete observation controls using the fixture's exact retained world:

```sh
SEEDBOX_ROOT="$PWD" CPU0="$CPU" VMM="$PWD/crates/seedbox-vmm/target/release/seedbox-vmm" bash harness/batch.sh "$RUN/output/world.json" trigger62-digest "1009" --digest
SEEDBOX_ROOT="$PWD" CPU0="$CPU" VMM="$PWD/crates/seedbox-vmm/target/release/seedbox-vmm" bash harness/batch.sh "$RUN/output/world.json" trigger62-trace "1009" --digest --trace
python3 - "$RUN/output/vmm.json" runs/trigger62-digest-s1009-r0.json runs/trigger62-trace-s1009-r0.json <<'PY'
import json, sys
rows = [json.load(open(p)) for p in sys.argv[1:]]
def semantic(r):
    fields = ('seed', 'counter_profile', 'outcome', 'sim_seconds', 'frames_delivered',
              'frames_dropped', 'network_counters', 'world_witness', 'kills', 'faults',
              'fault_error', 'control_history', 'control_latency_ns', 'triggers',
              'rejected_fault_effects', 'management_readiness', 'in_flight_policy')
    machine = ('name', 'state', 'guest_exit_code', 'guest_init_error', 'infrastructure_error',
               'time_ns', 'branches', 'pending_io', 'witness', 'state_digest',
               'state_digest_parts', 'net', 'control_channel', 'disk', 'incarnation',
               'booted_at_ns', 'crashed_incarnations')
    return ({k: r[k] for k in fields}, [{k: m[k] for k in machine} for m in r['machines']])
assert semantic(rows[0]) == semantic(rows[1]) == semantic(rows[2])
print('identical committed trigger, application, network, machine and world evidence')
PY
```

For the parent-engine unused-trigger regression, prepare a no-trigger world
once using the existing Redis fixture, then build the unmodified parent
separately (the world JSON contains absolute retained artifact paths):

```sh
sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" target/release/seedbox run examples/redis --kernel "$KERNEL" --cpu "$CPU" --state "$OUT/unused-state" --allow-dev-engine --full-state > "$OUT/unused.log"
NO_TRIGGER_RUN=$(sed -n 's/^[A-Z]*: //p' "$OUT/unused.log")
NO_TRIGGER_WORLD="$NO_TRIGGER_RUN/output/world.json"
PARENT_CHECKOUT=$(mktemp -d /tmp/seedbox62-parent.XXXXXX)
git archive 3af90bb^ | tar -x -C "$PARENT_CHECKOUT"
(cd "$PARENT_CHECKOUT" && cargo build-vmm)
SEEDBOX_ROOT="$PWD" CPU0="$CPU" VMM="$PARENT_CHECKOUT/crates/seedbox-vmm/target/release/seedbox-vmm" bash harness/batch.sh "$NO_TRIGGER_WORLD" trigger62-parent-unused "1009" --digest
SEEDBOX_ROOT="$PWD" CPU0="$CPU" VMM="$PWD/crates/seedbox-vmm/target/release/seedbox-vmm" bash harness/batch.sh "$NO_TRIGGER_WORLD" trigger62-new-unused "1009" --digest
```

Expected: both runs succeed, omit `triggers`, and match all of the semantic
fields listed above (remove only `triggers` from that projection for unused
reports). Compare every `*.console`/`*.mgmt` file with `cmp`. The comparison is
against the parent of the implementation commit; preserve that revision when
running later commits. This parent check has not run here.
