# Properties and oracles

These are interfaces only: the engine does not read guest property records yet,
so the guest library's calls do nothing, runs report no property findings and
an oracle's exit 3 still reads as failed.

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

Each call sends one JSON record over `SOCK_SEQPACKET` to `/run/seedbox/sdk.sock`.
The supervisor relays each as `SBX_PROP1 <session> <seq> <hex>`, uninterpreted,
and the engine answers `flush` on `SBX_SDK1 <session> <json>`. Records are at
most 4096 bytes, IDs 256 and kept details 1024. Times come from frame commit,
never the guest. The [records](../crates/seedbox-engine-protocol/src/property.rs)
are `require`, `assert` (kind `always` or `never`), `flush`, and `lost` for
records the guest library or supervisor could not deliver. For exploration,
`event` and `setup_complete` mark workload milestones, `state` reports that a
named piece of workload state took a short value, such as a role, and `choice`
asks the engine to pick one of up to 256 named alternatives; the session waits
for the answer, which the engine sends on the same line as a flush reply, and
until it does, `choose` takes the first alternative.

An evaluation always arrives: details too large for a record are dropped and
marked `details_truncated`. A record the library cannot send is counted and
reported as `lost` on the next session. `flush` fails when no reply with its own
sequence number arrives within 60 seconds of guest time; `choose` waits 10
seconds, then takes the first alternative and records a
`seedbox.choice_unanswered` event.

## Verdicts and crashes

Once property findings exist, a violation fails a completed run whatever the
oracle said, and a required property never evaluated fails a run whose oracle
exited. Lost or malformed evidence makes a passing oracle's run incomplete.
Findings arrive with the final report, so a passing oracle reads as pending
until completion. A failed run's status lists the violations, unmet
requirements and lost evidence as its reason. Until then the verdict stays the
oracle's; see [the policy](../crates/seedbox-api/src/status.rs).

Two default `never` properties need no guest library. On any machine,
`seedbox.no_out_of_memory_kills` is violated by each existing `SBX_OOM1` victim
report, and `seedbox.no_crashing_processes` by each process death from a
core-dumping signal, reported once per crash from the kernel's fatal-signal
line and relayed as `SBX_CRASH1`; segfault, trap and exception lines are not
relayed, since they precede the same crash's report. A machine whose processes
may crash or be killed by design opts out of both:

```yaml
machines:
  s3:
    image: example/keel
    allowCrashes: true
```

## Oracles

[seedbox-oracle](../crates/seedbox-oracle/src/lib.rs) holds the Jepsen-style
shapes an oracle fills in. `Client`s generate operations from a seeded `Random`
and perform them; a `History` records invoke/ok/fail/info lines with guest times.
A `Nemesis` applies the world's own
[control `Action`s](../crates/seedbox-engine-protocol/src/control.rs), with handles
it names, through the oracle's `World` while the clients run, then heals them.
`FinalRead` waits for recovery, restarts exited nodes and lets replicas converge
before reading each node. Each `Checker` turns the history and reads into a
`Verdict`, evaluated as an `always` property named by the checker. An undecided
verdict evaluates nothing and the oracle exits 3, which marks the run
incomplete:

```rust
impl Checker<Publish, Ack, Stream> for Queue {
    fn name(&self) -> &str { "acknowledged publishes survive on every node" }
    fn check(&self, history: &[Record<Publish, Ack>], reads: &[Read<Stream>]) -> Verdict {
        queue_check(history, reads) // lost, divergent and unexpected values
    }
}
```
