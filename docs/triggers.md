# Committed console triggers (development engine)

Issue #62 adds a deliberately small event surface: actual COM1 UART transmit
bytes (including debug port 0xe9) on one prepared machine. Application stdout
reaches this stream through the ordinary supervisor. Markers are opaque bytes;
no log JSON, protocol decoding, packet counts, host log tailing or application
commit inference is involved. COM2 and virtio management traffic are excluded.
Network/control event selectors and connection-selected triggers are deferred.

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

## KVM tests

The committed-trigger World tests are ignored KVM tests. On a Linux KVM host,
run them with `cargo test-vmm --features kvm-tests -- --ignored committed_trigger`,
and run `harness/acceptance/workloads/triggers.yaml` with `seedbox run` and
`seedbox replay` for the guest-owned fixture.
