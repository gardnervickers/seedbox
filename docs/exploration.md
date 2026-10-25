# Exploration

These are interfaces only. Every server, worker and command refuses them
("... is not implemented yet"); a later change implements them.

You own the scenario: the workload, the faults that matter, and the checks.
Exploration only searches what you wrote. Seedbox samples faults from a
profile you write, runs seeds and branches of your scenario, and narrows a
failure by rerunning your program with one thing changed. It never invents a
fault kind, a workload or a check, and it never shrinks a run.

## Fault profiles

A [profile](../crates/seedbox-engine-protocol/src/fault_profile.rs) is a
request option (`seedbox run --fault-profile FILE`, a seed range, a
campaign), never part of a definition. Each entry is an action with ranges
in place of values, under the action's own snake_case names, and is checked
by that action's own validator:

```yaml
start_ns: 2000000000
end_ns: 60000000000
interval_ns: 5000000000
targets: [db1, db2, db3]
faults:
  - {kind: partition, weight: 3, duration_ns: {min: 1000000000, max: 10000000000}}
  - {kind: crash, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: disk_latency, op: flush, duration_ns: {min: 1000000000, max: 5000000000},
     latency_ns: {min: 1000000, max: 50000000}}
```

A seeded sampler in `seedbox-engine-core` draws it once, from the run's
seed, into ordinary program inputs: fault `n` starts at key `gen-<n>` and its
heal, resume, reboot or restart is `gen-<n>-end`. Each seed-range member
samples with its own seed. Submission checks that the program has room for
every input the profile can generate. The resolved definition keeps the
generated inputs, so a replay never samples again. A generated start
is skipped, with a rejection saying why, before every initial application
has reported ready (the Driver's readiness latch) and while a quiet period
is in force; its teardown is skipped with it. The oracle's `quiet` call
(`World.quiet(seconds)` in Python) starts a quiet period; teardowns of
running faults still happen.

Engines gate inputs by key, so the prefixes `gen-`, `mutate-` and
`campaign-` belong to Seedbox: a project's own overrides may not use them.
Programs Seedbox retains keep them; repeat such a run with `replay` rather
than by resubmitting its definition as a project.

## Branch points and choices

Three [program inputs](../crates/seedbox-engine-protocol/src/input_program.rs)
take no fault ID. An `anchor` marks the checkpoint phase at its time, before
the other inputs due then. The `captureAnchors` execution option captures
checkpoint `anchor.<key>` there, under main's own capture rules: every
initial application has been ready, no injected fault is prepared and no
other capture is pending. An anchor where they forbid a capture passes
uncaptured, never later, and its receipt records the reason; nothing can
branch from it. A `reseed` after its anchor re-keys every seeded engine
stream from then on. A `choice` input selects the answer to one guest
choice, named exactly as the engine counts it: machine, incarnation, SDK
session, site and the site's occurrence in that session, from 1.

```yaml
overrides:
  - {id: a0, atNs: 8000000000, action: {action: anchor}}
  - {id: a0-reseed, atNs: 8000000000, action: {action: reseed, seed: 7, identity: member/4}}
  - {id: pick, atNs: 8000000000,
     action: {action: choice, machine: client, incarnation: 0, session: 1,
              site: retry, occurrence: 2, select: 1}}
```

What a reseed varies is narrower than "everything after T". It re-keys the
Driver's per-machine streams (boot entropy for later reboots), seeded
clock-fault values, every per-link network stream (loss, jitter, reorder,
duplication, burst loss, spikes, resets), and the seeded disk, preemption
and choice streams. It does not touch guest memory: the guest kernel's
random pool and every application PRNG carry over unchanged, and neither
backend feeds a running guest entropy (PV passes boot entropy in the device
tree, TCG a boot seed to its QEMU process). A guest's own randomness
therefore changes only through a reboot or through the different timing
that re-keyed host draws cause, and a run with no seeded network, fault or
choice draws after the reseed may not vary at all.

Guests ask with `seedbox_sdk::choose(site, &alternatives)`; the engine draws
the answer from the seed unless a choice input selects it, so choices
replay and a branch can vary them. The SDK waits ten seconds of guest time
for an answer, then takes the first alternative, reopens its session and
records a `seedbox.choice_unanswered` event. `seedbox_sdk::state(name,
value)` reports short labels, such as a role or recovery phase, whose new
values and transitions count as progress. Typed triggers can also match SDK
`event`, `assertion`, `state` and `setup_complete` records, on any machine
unless one is named, and `fired` (an earlier trigger input's effect being
admitted); see [faults](faults.md).

## Campaigns

A [campaign](../crates/seedbox-api/src/campaign.rs) is a seed range whose
later members a planner chooses (`seedbox campaign`). Generation 0 runs roots
with consecutive seeds and the plan's anchors, keyed `campaign-anchor-<n>`.
Each later generation is planned once the whole previous one has finished,
from every result in member order, so plans never depend on worker counts or
which member finished first;
[`Generations`](../crates/seedbox-explore/src/lib.rs) enforces that for any
[`Planner`](../crates/seedbox-explore/src/lib.rs). A branch continues a
member from one of its anchor captures with a reseed, and may select choices
or place one fault after something its parent did: a `mutate-<m>` trigger
input and its `fired` teardown. Each mutation takes two of the run's 64
trigger IDs and a branch keeps its ancestors', so a lineage holds at most 32
mutations, fewer if the program arms triggers of its own. Like a seed range,
a campaign is addressed by its first root, and branched members name their
parent and checkpoint in their metadata.

Findings group failing members by property, split by any `groupBy` detail
fields; each names its earliest member as the representative. With a
`baseline` campaign, findings are labelled new or observed again, and
baseline groups this campaign did not show are listed.

## Narrowing

`seedbox analyze` reruns a failed simulation's own program under derived
seeds, in arms: every draw varied; the engine's draws varied only after a
time (an anchor and reseed added there, with the limits above); and, with
`--faults`, the program without one fault and its teardown, as `derive`
leaves entries out. Each arm reports how often the run fails the same way,
with an interval: a measured rate, not a cause. `seedbox explain` lays the
retained evidence of one run on one simulated-time line, with links between
entries and a list of what the evidence omits.

`seedbox checkpoint ID --cold` will write the inputs to rerun a simulation
from the start: its whole program, a branch's consumed prefix first, over
its root's preparation. Replay it before relying on it: a cold rerun equals a
branch only where execution from a capture equals execution from the start.

## Refusals

| Input | Where it is refused | Error |
| --- | --- | --- |
| `faultProfile`, `captureAnchors` | submission (`POST /v1/simulations`, seed ranges) | `notImplemented` (501), after the profile's own checks |
| `gen-`, `mutate-`, `campaign-` keys in a project's overrides | submission | `invalidRequest` |
| `anchor`, `reseed`, `choice` inputs | submission (`notImplemented`, 501), then worker open | `program KEY: anchor is not implemented yet` |
| triggers on SDK evidence or `fired` | as every program trigger | `program KEY: trigger is not implemented yet` |
| oracle `quiet` | the oracle service | `unsupported`: `quiet is not implemented yet` |
| campaigns, analyses, explanations, cold bundles | their endpoints | `notImplemented` (501) after request checks; `notFound` for an unknown simulation |
| `campaign`, `analyze`, `explain`, `checkpoint --cold`, `run --fault-profile` | the CLI, before connecting | `... is not implemented yet`, status 2 |

Until the engine serves the SDK socket, `choose` returns the first
alternative and `state` does nothing, as outside Seedbox.
