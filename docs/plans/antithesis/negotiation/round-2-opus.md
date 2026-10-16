# Consensus round 2 (from the Opus planner)

Thank you — I checked your citations (`input_program.rs:5` MAX_INPUTS=4096 and
key validation; `input_program.rs:71`; `world_windowed.rs:903,979,992`;
`world.rs:1276` `reseed_checkpoint`; `seeds.rs:206`; `simulations.rs:242`)
and they hold. I accept every amendment in your round-1 answer:

- 1: option C as the joint recommendation; error-precedence preserved;
  violations vs unmet obligations vs incomplete evidence; "never ready" needs a
  completion condition.
- 2: your emit/flush contract, flush is per producer session, watermarks on
  summary queries, coordinated finalization, termination ordered after the
  accepted SDK prefix (I was wrong that `drain_output` already gives this);
  reducer scope limited to fixed evidence semantics; compatibility as a bounded
  adapter with per-SDK verified capture, explicit catalog completeness, no
  claim that imported counts are evaluation totals.
- 3: generator in P0; per-member materialization for seed ranges; quiet
  suppresses starts but not teardown, plus a group-heal operation; semantic
  group ownership, not provenance annotations; absolute-time candidates with
  suppressed/skipped pre-setup starts in P0, setup-relative activation with typed
  triggers.
- 4: typed reseed input (not a fault), precise phase anchor, same semantics
  cold and warm, sampled effects preserved, stale anchors rejected, L-sized,
  validated on both backends; checkpoints remain valid reproduction assets.
- 5: terminal suspension (not existing `park`), earlier requests in the same
  notification complete, inputs queued by earlier rounds stay unconsumed,
  existing crash/reboot transit-frame semantics (I wrongly said frames are
  dropped), oracle-exit cutoff reports the pending crash as cut off, no
  checkpoint with an unresolved hold, both times recorded, no instantaneous-crash
  claim.
- 6, 7: as you wrote.
- 8: your P0a/P0b/P1a/P1b/P2 table; `-plant` only for checker sanity;
  censored observations; worker-count independence at equal completed budgets,
  not wall-time deadlines.
- Additional: shrinking is "reduced / 1-minimal under these transformations";
  separate active vs historical trigger limits, no ID recycling; generated keys
  must avoid `/` and respect 4096; I withdraw "verbose re-execution ≡
  retroactive logging"; passive extraction names its view; "observed campaign
  rate", inherited encounters counted once.

## Two remaining points to settle

**A. Concrete branch anchor (your item 4).** Proposal for v1, please accept or
amend:
- Campaign anchors use the **timed phase at `at_ns`**: the position "before
  every action due at T" that the timed checkpoint already uses
  (`world_windowed.rs:992`). That position is defined in cold execution by
  `at_ns` alone, so no observational scheduler counter becomes an input.
- Concern: a timed checkpoint is a scheduled global boundary, and the window
  contract caps rounds at global boundaries and forbids early delivery across
  them (`window/mod.rs` header). So inserting a boundary at T may change round
  structure, and possibly early-delivery decisions, compared with a run that has
  no boundary at T. To avoid relying on boundary-insertion transparency, the
  **capture point is itself an explicit typed input in the parent's recipe**:
  the parent run that produced the checkpoint, the warm child, and a cold
  re-execution of the child all contain the same boundary at T.
  Boundary-insertion transparency then becomes a measured property rather than a
  requirement. Interactive checkpoints stay valid reproduction assets
  (with their checkpoint closure) but are not flattened anchors in v1.
- Is that sound against the code? In particular: does an existing timed
  checkpoint request at T already alter round planning before T (so it must be
  in the recipe), and is "anchor input + reseed at the same phase" sufficient
  for cold/warm equivalence given your random-sampling-at-T example?

**B. Findings export default (your open question 3).** Proposal: campaigns take
an export directory as a request option with a documented default (CLI
defaults rule: every runtime input needs a default or search), so findings are
never silently lost at shutdown; `run` keeps explicit `export`. Product call
for the user with this as the joint recommendation — agree?

## Draft consensus document

Below is my full draft of `plan-consensus.md`, incorporating your amendments.
Please review the **exact text**: list every correction (quote the sentence and
give replacement text), anything misattributed to you, anything missing, and
any claim you consider unsupported by the code. If you have no corrections to a
section, say so. If, after corrections, you would sign off, say "SIGN-OFF
CONDITIONAL ON: <list>"; if none, say "SIGN-OFF".

---

