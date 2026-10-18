# Consensus round 1 (from the Opus planner)

You wrote `plan-codex.md`. Another planner (Claude Opus) independently wrote a
plan from the same brief; its full text is appended at the end of this message
under "APPENDIX: plan-opus.md". The user wants the two of us to reach **full
consensus on one Seedbox plan and its sequencing**. Rules: argue on the merits
with evidence from the repo (ec80ea9f, read-only) and the Antithesis docs;
concede when the other side is right, push back when it isn't; don't trade
concessions. Anything that is genuinely a product decision for the user (e.g.
exit-code semantics) we settle by agreeing on the options, trade-offs and a
joint recommendation. No repo edits.

## Facts the coordinator verified at ec80ea9f

- `park_for_fork` exists at `crates/seedbox-engine-core/src/world_windowed.rs:1174`.
- `Block::notify` (`virtio_block.rs:172`) completes requests synchronously.
- Live checkpoint holding is capped at 8 (`checkpoint.rs:127,159`).
- The block device negotiates FLUSH and **no FUA** (`crates/seedbox-vmm/src/virtio_blk.rs:221`); `docs/internals.md:320` mentioning FUA is doc drift. (My plan wrongly mentioned FUA in passing; I accept your correction.)
- `MAX_TRIGGERS = 64` (`crates/seedbox-control/src/lib.rs:197`).
- `observation.rs:1`: live observation delivery is lossy and must not affect results.

## My positions after reading your plan, item by item

For each item: where I now stand, what I concede, and what I still contest.
Please answer each numbered item explicitly with AGREE / DISAGREE / AMEND and
your reasons (cite code where it matters). Add any disagreement I missed.

### 1. Does a failing property make `run` FAIL?

I concede your framing: today's contract is that the oracle exit code is the
verdict (README; `status.rs:55`), and changing that is a user decision, not an
implementation detail. Proposed joint position:
- Keep `TestVerdict` oracle-only. Add a separate `PropertyAssessment`
  (violations / unmet `sometimes`/`reachable` goals / incomplete) on every
  simulation and campaign.
- `explore`/campaigns report and exit on the assessment (FAIL if any safety
  violation: `always`, `alwaysOrUnreachable`, `unreachable`, default crash/OOM
  properties). Unmet `sometimes` is reported, not FAIL, unless requested.
- For `run`: present the user with options (A) exit code stays oracle-only and
  the assessment is printed as extra `key: value` lines; (B) exit 1 if the
  oracle fails or any safety property is violated; (C) A by default plus a
  presence flag `--fail-on-violation` (changes exit status only, never
  results, so it is allowed as a flag). Joint recommendation: C now, revisit B
  once properties are established. Agree?

### 2. Guest→engine property transport, and Antithesis compatibility

Where we already agree: correctness must never come from the lossy live
observation stream; records are accepted from **committed** guest bytes in the
engine at a deterministic position (my plan said exactly this: "fed only from
committed management bytes, the same rule as triggers.rs"); available on every
machine, without granting control privileges (control admission is
controller-only, `control.rs:554`); bounded frames; attribution to machine,
incarnation, process, local sequence; partial frames and reducer state in
checkpoints; capacity exhaustion is explicit, never silent loss.

I concede two points:
- A plain FIFO carrying JSONL is not a sound native transport: concurrent
  writers with lines > PIPE_BUF (4 KiB) can interleave. The native transport
  should be framed: e.g. a `SOCK_SEQPACKET` Unix socket `/run/seedbox/sdk.sock`
  served by the supervisor (`compose-init.c`), which relays each message as a
  distinct `sdk` frame type on the existing management port (alongside the
  application-output frames, `compose-init.c:489`), not on the control channel.
- Reduction should not be offline-only. If oracles can query a committed
  summary (your design), the engine needs deterministic reducer state. Proposal:
  the record format and a pure reducer live in `seedbox-engine-protocol`
  (evidence-format semantics, no policy), used both by engine-core (checkpointed
  live state, queried by a controller op) and by `seedbox-artifacts` (offline
  reports), so the two cannot disagree.

I contest: **per-record engine acknowledgement.** A host round trip per
assertion adds latency and simulated-time cost to the hot path and changes
timing whenever properties are added; Antithesis assertions are fire-and-forget
and "can run safely in production". Records lost because the machine crashed
before they were committed are lost deterministically (same on replay), which
is the honest semantics of a crash. Proposal: no per-record ack; an explicit
`sdk.flush()` barrier (round trip) for callers that need certainty, and the
supervisor already drains outputs at termination (`drain_output`,
`compose-init.c:497`). If you think ack-per-record is needed, what bug or
evidence gap does it close that the barrier doesn't?

Antithesis compatibility: I propose the native wire format is Seedbox's own
(versioned), and we **additionally** accept Antithesis fallback-schema JSONL
(`reference__sdk__fallback__schema.md`) through an adapter, because existing
Antithesis SDKs in Go/Rust/Python/Java/C++ write it to
`ANTITHESIS_SDK_LOCAL_OUTPUT` when not in Antithesis (`reference__sdk__rust.md:75`,
`reference__sdk__go.md:94`). That gives multi-language assertions on day one at
small cost (a line-oriented compat endpoint with the PIPE_BUF caveat recorded
as malformed-line evidence). Env-var injection would be per-machine opt-in,
visible in the resolved definition, and the naming/trademark question goes to
the user. Same for an `/opt/antithesis/test/v1` layout in a test-template
runner (directory configurable; Seedbox default path). Do you accept
compatibility as an adapter, or do you think it is harmful? Why?

### 3. Seeded fault generator: P0 standalone vs only inside campaigns

My position: a seeded generator that **materialises keyed overrides into the
resolved definition** (so replay never reruns it; one override timeline;
editable in the TUI) should exist before guided campaigns, usable as
`run --faults profile.yaml` and with the existing seed-range `explore`.
Arguments: (a) "faults random by default, continuously" is the first thing
Antithesis does (`product__writing_tests__controlling_faults.md`) and the
docs say many bugs are found with just a workload + default faults
(`product__writing_tests.md`); (b) campaigns need exactly this component to
generate branch suffixes, so it's not throwaway; (c) it is M-sized and needs no
checkpoints. Seed sweep + generated faults + properties = a useful product
before the XL campaign lands. What's your argument for deferring it into
campaigns only?

### 4. Branch representation: `reseed` override + flattening vs journaled recipes

I think these converge. Proposal:
- A branch recipe is expressed **in the one override timeline**: the parent's
  consumed prefix + a new `reseed{identity}` override at the branch position +
  the new suffix. The coordinator applies `reseed` between rounds, reseeding
  host RNG domains, timer/progress jitter and guest entropy exactly as the
  current checkpoint branch does (`engine_adapter/world.rs:250-256`,
  `world.rs:1257-1265`, `kernel/apply.py:190`). Existing `Checkpoint{branch_seed}`
  branching is re-expressed as this action so there is one implementation.
- Execution uses the parent checkpoint when available (`Start::Checkpoint`,
  `validate_overrides` already protects the consumed prefix).
- The same recipe is **also** runnable cold (`Start::Fresh` + identical
  overrides). That requires a new, testable invariant: *capture/restore is
  transparent* — a cold run with `reseed` at position P equals checkpoint-at-P +
  branch. I found no test asserting this today (closest:
  `world_windowed_tests.rs:1246,2485`, `checkpoint_feature_off.rs:58`), so it
  is validation work, not an existing guarantee. Where it holds, findings need
  no RAM checkpoint assets in bundles and shrinking can delete reseeds; where
  it fails, it's a determinism bug worth knowing about.
- Bundles may still include the checkpoint closure as a cache.
Is there a reason the journaled recipe should *not* be an override-timeline
entry? Is there a reason cold re-execution should not be a required invariant?

### 5. Block-event crash timing

I concede that my "commit the crash at the recorded mid-round time" was an
unnecessary claim. But I think your mechanism is also heavier than needed.
Proposal (synthesis): a device-local rule matches the request at admission in
`Block::notify` **without completing it** (no disk mutation, no used-ring write,
no IRQ). The machine then **parks at that exit** — the same kind of early stop
`park_for_fork` performs — and executes nothing further in the round. At the
next committed boundary the coordinator commits the crash, with effect time =
that boundary (honest time, coordinator owns lifecycle at its commit site).
Why this is safe in the windowed scheduler (`docs/internals.md` "Parallel
scheduling"; `world_windowed.rs`): a round is bounded by minimum network
delay, so no frame produced in the round is delivered before the commit;
frames the machine sent before parking keep their routing; frames addressed to
it are due after the commit and are dropped as receiver-down; the parked
machine's guest state is frozen, so crashing it at the boundary is
observationally identical to crashing it at the park instant, except that the
recorded time is the boundary. The held request is never completed, so the
volatile layer at crash is exactly the pre-request state.
This needs no asynchronous block device. Async pending requests are still
needed for latency (see 6), but not for this crash. Do you see a hole? (E.g. a
lifecycle commit-site rule, owed I/O, checkpoint "no owed I/O" constraints,
or ARM differences.)

### 6. Storage scope and mechanism

I concede:
- **No lying flush.** Software cannot be correct if acknowledged flushes are
  not durable; it would mostly report expected loss. Drop it.
- **Latency via "owed I/O" is wrong semantics.** Charging time to the machine
  stalls the single vCPU like synchronous I/O, so other threads cannot run while
  the request is outstanding. Delayed completion needs true pending requests
  with completion deadlines, as you propose.
- Corruption of durable data is a distinct contract, not crash loss.
Remaining proposal: P0/P1 storage = read/write/flush IOERR rules (synchronous,
using the existing `S_IOERR` path and completed-prefix semantics) + the
held-request crash from item 5. Async pending requests + delayed completion
are a later L item. Silent read corruption I would keep as a *later, opt-in*
item rather than excluded outright: single-replica corruption propagating
through replication is a documented real-world bug class (Ganesan et al.,
FAST'17, "Redundancy Does Not Imply Fault Tolerance"); do you accept it as P2
opt-in, or still exclude it?

### 7. Coverage

I partly concede: guidance should start with assertions/semantic milestones,
and coverage should be justified by measured yield. Also, coverage must be a
resolved-definition setting (never a performance flag or env var), because
translator allocation and syscalls are charged and could change results; I
withdraw "always on". Proposal: P1 includes a bounded *measurement spike*
(prototype module-relative edge bitmap in the DynamoRIO client, read by the VMM
at stopped boundaries) evaluated on fixed workloads with known bugs; the full
feature lands in P2 only if yield justifies it. Agree?

### 8. Sequencing

Proposed merged sequence (please amend):

- **P0 — one run yields properties and a durable, reproducible finding.**
  (1) property transport + record format + reducer + `get properties`;
  (2) default properties via guest-kernel/supervisor events (fatal signals,
  OOM kill, unexpected exits, never-ready);
  (3) durable findings + bundle export/import after server shutdown (your P0
  point; I agree session-only loss of findings is unacceptable);
  (4) Rust oracle toolkit (readiness, structured choices, bounded parallel
  work, quiet/recovery phases, property summary) — includes `quiet` control op;
  (5) seeded fault generator materialised into overrides (contested, item 3);
  (6) storage IOERR rules.
  Exit criteria: on fixed workloads with planted bugs (e.g. the etcd `-plant`
  sanity control), a run reports the violation, exports a bundle, and a fresh
  server imports it and replays to MATCH with the same property violation.
- **P1 — guided campaigns.** `reseed`/recipe representation (item 4); campaign
  planner (`seedbox-explore`) with generation-synchronous, worker-count-
  independent planning; checkpoint byte accounting replacing the cap of 8;
  typed triggers (SDK events, block ops, lifecycle, connection) incl.
  held-request crash; findings search/query; shrinking (ddmin over external
  overrides only); CI/agent workflow; coverage measurement spike.
  Exit criteria: campaign beats seed sweep + generator on time-to-first-failure
  for ≥2 fixed workloads with planted bugs, with identical findings across
  worker counts.
- **P2.** In-worker snapshot tree (only after measuring capture/restore cost,
  as you say); coverage feature (if spike justifies); seeded preemption /
  thread perturbation; async pending block requests + delayed completion;
  causality resampling (with sample counts and uncertainty, not "proof");
  debug exec on child branches; passive file extraction from retained disk
  layers; test-template runner; optional durable-data corruption.

Please respond item by item, then give your own proposed sequencing if it
differs.

---

APPENDIX: plan-opus.md (full text)

