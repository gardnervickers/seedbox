# Properties and oracles

## Guest properties

A program on any machine links [seedbox-guest](../crates/seedbox-guest/src/lib.rs):

```rust
seedbox_guest::require("failover exercised"); // at startup: must be evaluated
seedbox_guest::always("acked writes survive", ok, json!({"key": 7}));
seedbox_guest::never("two leaders", leaders > 1, json!({"leaders": leaders}));
seedbox_guest::flush()?; // before the oracle exits
```

`always` is violated by a false evaluation and `never` by a true one; a single
violation fails the run. Properties are optional to hit unless `require`d, and
a required property that no machine ever evaluates fails the run at completion.
The engine keeps only failures: each violated property with its first example,
unmet requirements, and lost evidence. There is no per-property tally.

Each call sends one JSON record over `SOCK_SEQPACKET` to `/run/seedbox/guest.sock`.
The supervisor relays each as `SBX_PROP1 <session> <seq> <hex>`, uninterpreted,
and the engine answers `flush` on `SBX_GUEST1 <session> <json>`. Records are at
most 4096 bytes, IDs 256 and kept details 1024. Times come from frame commit,
never the guest. The [records](../crates/seedbox-engine-protocol/src/property.rs)
are `require`, `assert` (kind `always` or `never`), `flush`, and `lost` for
records the guest library or supervisor could not deliver.

An evaluation always arrives: details too large for a record are dropped and
marked `details_truncated`. A record the library cannot send is counted and
reported as `lost` on the next session. `flush` fails when no reply with its own
sequence number arrives within 60 seconds of guest time.

The supervisor exports the socket's path as `SEEDBOX_GUEST_SOCKET`, so the
library can tell running outside Seedbox from a socket it cannot reach (a mount
over `/run`, say): then it connects to the supervisor's abstract socket
`seedbox-guest`, and if that fails too, it reports its records lost on
`/dev/kmsg`, which the supervisor relays, so the evidence reads incomplete.

The engine reduces records at commit, in commit order, and answers a `flush`
only after reducing every earlier record of that session, on the same boot.
Every record committed before the run ends is reduced, including other
machines' records in the oracle's last round, and the supervisor reports each
session that ends. Records still inside a guest, in a program's socket or the
supervisor, when its machine crashes or the run ends are physically lost with
it: flush before relying on them.

## Verdicts and crashes

A violation fails a completed run whatever the oracle said, even one whose
oracle never exited. A required property never evaluated fails a run whose
oracle exited; a run cut short before then stays as the oracle left it. Lost or
malformed evidence makes a passing oracle's run incomplete (exit status 3), as
does an oracle's own exit 3. Findings arrive with the final report, so a
passing oracle reads as pending until completion; see
[the policy](../crates/seedbox-engine-core/src/properties.rs). `seedbox show ID`
lists a failed run's violations, unmet requirements and lost evidence as its
reason; `--json` status documents carry them as `properties`.

Two default `never` properties need no guest library. On any machine,
`seedbox.no_out_of_memory_kills` is violated by each `SBX_OOM1` victim report,
the application's or any other process's, and `seedbox.no_crashing_processes`
by each process death from a core-dumping signal, reported once per crash from
the kernel's fatal-signal line and relayed as `SBX_CRASH1`; segfault, trap and
exception lines are not relayed, since they precede the same crash's report. A
machine whose processes may crash or be killed by design opts out of both:

```yaml
machines:
  s3:
    image: example/keel
    allowCrashes: true
```

## Oracles

[seedbox-oracle](../crates/seedbox-oracle/src/lib.rs) holds the Jepsen-style
shapes an oracle fills in, and the runner that drives them (`run`, or `main` for
an oracle binary). `Client`s generate operations from a seeded `Random`
and perform them; a `History` records invoke/ok/fail/info lines with guest times.
A `Nemesis` applies the world's own
[control `Action`s](../crates/seedbox-engine-protocol/src/control.rs), with handles
it names, through the oracle's `World` while the clients run, then heals them.
`FinalRead` waits for recovery, restarts exited nodes and lets replicas converge
before reading each node. The library also has an in-memory `Recorder`, a seeded
`Rng`, the control socket's `GuestWorld` (which maps the handles it is given to
the world's own), the standard fault `patterns` (kill/restart, process pauses,
partitions, crash-all) and the `queue` checker. Each `Checker` turns the history
and reads into a `Verdict`, evaluated as an `always` property named by the
checker. An undecided verdict evaluates nothing and the oracle exits 3, which
marks the run incomplete:

```rust
impl Checker<Publish, Ack, Stream> for Queue {
    fn name(&self) -> &str { "acknowledged publishes survive on every node" }
    fn check(&self, history: &[Record<Publish, Ack>], reads: &[Read<Stream>]) -> Verdict {
        queue_check(history, reads) // lost, divergent and unexpected values
    }
}
```

### Fault profiles

A [`nemesis::Profile`](../crates/seedbox-oracle/src/nemesis/profile.rs) is a
nemesis written as data, which the oracle ships as YAML or JSON: which kinds of
fault, on which machines, in what window, how often and with what ranges.

```yaml
start_ns: 2000000000      # after Nemesis::run begins
end_ns: 60000000000
interval_ns: 5000000000   # each gap is drawn from half to 1.5 times this
targets: [db1, db2, db3]  # max_faults defaults to 64
faults:
  - {kind: partition, weight: 3, duration_ns: {min: 1000000000, max: 10000000000}}
  - {kind: crash, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: clock_skew, duration_ns: {min: 1000000000, max: 5000000000},
     max_offset_ns: 30000000000}
```

Each entry is named by the action it starts and carries that action's fields,
with `{min, max}` ranges for drawn values. Each start draws an entry by weight,
a target with no fault of the profile in force (or skips the start when there is
none), then the entry's values. It applies the fault through `World::apply` and
ends it with the action's counterpart after its drawn duration, so faults may
overlap. Every draw comes from the scenario's `Random`, so a seed fixes the
applied sequence. `run` checks every entry with its action's own validator
before applying anything, and refuses an invalid entry with that action's
message. `heal` ends every fault still in force. A partition cuts its target off
from `from`, and a shape delays traffic to `to`; each defaults to the other
targets. The world checks what needs the world, such as the network's latency
floor, when the action is applied. See
[the acceptance oracle](../harness/acceptance/images/profile/src/main.rs).
