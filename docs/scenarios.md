# Properties and scenarios

These are interfaces only: the engine does not read guest property records yet,
so the SDK's calls do nothing and runs report no property results.

## Guest properties

A program on any machine links [seedbox-sdk](../crates/seedbox-sdk/src/lib.rs) and
sends one JSON record per call over `SOCK_SEQPACKET` to `/run/seedbox/sdk.sock`.
The supervisor relays each as `SBX_PROP1 <session> <seq> <hex>`, uninterpreted,
and the engine answers `flush`, `summary` and `choice` on
`SBX_SDK1 <session> <json>`; until it does, `choose` takes the first
alternative.
Records are at most 4096 bytes; IDs 256, messages and kept details 1024.
Times come from frame commit, never the guest. The
[records](../crates/seedbox-engine-protocol/src/property.rs) are:

| Record | Meaning |
| --- | --- |
| `assert` `always` | Every evaluation holds, and one happens |
| `assert` `always_or_unreachable` | Every evaluation holds |
| `assert` `sometimes` | Some evaluation holds |
| `assert` `reachable` / `unreachable` | Reached at least once / never |
| `declare` | Report a property even if never evaluated |
| `event`, `setup_complete` | Counted workload milestones |
| `state` | A named piece of workload state took a short value, such as a role |
| `choice` | Ask the engine to pick one of up to 256 named alternatives; the session waits for the answer |
| `flush`, `summary` | Wait until the session's earlier records are committed; `summary` adds counts |
| `lost` | Records the SDK or supervisor could not deliver; an evidence error |

An evaluation always arrives: details too large for a record are dropped and
marked `details_truncated`. A record the SDK cannot send is counted and reported
as `lost` on the next session. `flush` and `summary` fail when no reply with
their own sequence number arrives within 60 seconds of guest time.

## Verdicts and defaults

Once property results exist, a violated property fails a completed run whatever
the oracle said, and lost or malformed evidence makes a passing oracle's run
incomplete. Results arrive with the final report, so a passing oracle reads as
pending until completion. Unmet goals, such as a `sometimes` that never held,
never change a single run's verdict. Until then the verdict stays the oracle's;
see [the policy](../crates/seedbox-api/src/status.rs).

Two default properties need no SDK. `seedbox.no_out_of_memory_kills` is evaluated
on each existing `SBX_OOM1` victim report. `seedbox.no_crashing_processes` is
evaluated once per crash, on the kernel's fatal-signal report for a core-dumping
signal, relayed as `SBX_CRASH1`; segfault, trap and exception lines are not
relayed, since they precede the same crash's report. Each fails unless the
machine expects it:

```yaml
machines:
  s3:
    image: example/keel
    expectedCrashes:
      - {signal: SIGABRT, command: keel-watchdog}
    expectedOutOfMemoryKills: true
```

## Scenarios

[seedbox-scenario](../crates/seedbox-scenario/src/lib.rs) holds the Jepsen-style
shapes an oracle fills in. `Client`s generate operations from a seeded `Random`
and perform them; a `History` records invoke/ok/fail/info lines with guest times.
A `Nemesis` applies the world's own
[control `Action`s](../crates/seedbox-engine-protocol/src/control.rs), with handles
it names, through the oracle's `World` while the clients run, then heals them.
`FinalRead` waits for recovery, restarts exited nodes and lets replicas converge
before reading each node. Each `Checker` turns the history and reads into a
`Verdict`, reported as an `always` property named by the checker. An undecided
verdict leaves the property unmet and the oracle exits 3, which today makes the
run's verdict failed:

```rust
impl Checker<Publish, Ack, Stream> for Queue {
    fn name(&self) -> &str { "acknowledged publishes survive on every node" }
    fn check(&self, history: &[Record<Publish, Ack>], reads: &[Read<Stream>]) -> Verdict {
        queue_check(history, reads) // lost, divergent and unexpected values
    }
}
```
