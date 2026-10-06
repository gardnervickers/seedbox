"""Acceptance checks (issue #4, items 1-7).

Each check is a generator: it yields a list of jobs, receives their results in
the same order, and may yield again before returning a `Verdict`. The engine
batches every check's pending jobs together so a tier runs in as few parallel
rounds as possible.
"""
from dataclasses import dataclass, field
import re

from .model import EXITS, FAIL, FULL_STATE, PASS, UNRESOLVED, Job, performance, witness_differences

SKIPPED = "skipped"
IDENTITY_CHANGED = "identity_changed"
BASELINE = "baseline"


@dataclass
class Verdict:
    status: str
    summary: str
    details: dict = field(default_factory=dict)


@dataclass
class Context:
    pins: dict
    # Current hashes of each scenario's semantic inputs, from the launcher.
    identity: object = None
    # Declared by the person running the suite.
    semantics_change: bool = False
    performance_only: bool = False


def default_job(scenario, seed):
    """The default-settings run of a pinned seed, shared between checks."""
    return Job(scenario, seed, share=f"{scenario}/{seed}/default")


def unresolved(results):
    bad = [r for r in results if not r.completed]
    if bad:
        return Verdict(UNRESOLVED, f"{len(bad)} run(s) did not complete: " +
                       "; ".join(f"{r.job.describe()}: {r.detail}" for r in bad))
    return None


def replay(ctx):
    """Item 1: two fresh processes match on every witness, with and without digests."""
    pin = ctx.pins["replay"]
    scenario, seed = pin["scenario"], pin["seed"]
    results = yield [default_job(scenario, seed), Job(scenario, seed),
                     Job(scenario, seed, FULL_STATE), Job(scenario, seed, FULL_STATE)]
    if (v := unresolved(results)):
        return v
    compared = {
        "exit witnesses (world + per machine)": results[0:2],
        "exit witnesses + RAM/register digests": results[2:4],
    }
    details, failures = {}, []
    for what, (a, b) in compared.items():
        diffs = witness_differences(a, b)
        details[what] = {"machines": sorted(a.machine_witnesses), "differs": diffs}
        if a.job.witness == FULL_STATE and not (a.state_digests and b.state_digests):
            failures.append(f"{what}: no digests reported")
        if diffs:
            failures.append(f"{what}: differs in {', '.join(diffs)}")
    if failures:
        return Verdict(FAIL, "replay mismatch: " + "; ".join(failures), details)
    return Verdict(PASS, f"{scenario} seed {seed}: compared " + " and ".join(compared), details)


def stop_placement(ctx):
    """Item 2: performance-only stop settings never change the witness."""
    pin = ctx.pins["stop_placement"]
    scenario, seed = pin["scenario"], pin["seed"]
    variants = [performance(**v) for v in pin["variants"]]
    results = yield [default_job(scenario, seed)] + [Job(scenario, seed, performance=v) for v in variants]
    if (v := unresolved(results)):
        return v
    base, details, failures = results[0], {}, []
    for result in results[1:]:
        label = " ".join(f"{k}={v}" for k, v in result.job.performance)
        if result.retries and dict(result.job.performance).get("margin"):
            return Verdict(UNRESOLVED, f"{label} was not exercised: it completed only after an "
                           f"overshoot retry at a larger margin ({result.attempts[-1]['detail']})")
        diffs = witness_differences(base, result)
        details[label] = {"differs": diffs, "retries": result.retries}
        if diffs:
            failures.append(f"{label} differs in {', '.join(diffs)}")
    if failures:
        return Verdict(FAIL, "determinism bug: " + "; ".join(failures), details)
    return Verdict(PASS, f"{scenario} seed {seed}: identical witness under " + ", ".join(details), details)


RACE = re.compile(r"RACE mode=(\w+) counter=(-?\d+) expected=(-?\d+) lost=(-?\d+)")


def race_outcomes(result, machine):
    found = {m[0]: int(m[3]) for m in RACE.findall(result.logs.get(machine, ""))}
    return found.get("racy"), found.get("atomic")


def race(ctx):
    """Item 3: seeds vary a lost-update race; the atomic control never loses; a failure replays."""
    pin = ctx.pins["race"]
    scenario, machine = pin["scenario"], pin["machine"]
    results = yield [Job(scenario, seed) for seed in pin["seeds"]]
    if (v := unresolved(results)):
        return v
    lost, problems = {}, []
    for result in results:
        racy, atomic = race_outcomes(result, machine)
        if racy is None or atomic is None:
            problems.append(f"seed {result.job.seed}: missing RACE output")
        elif atomic != 0:
            problems.append(f"seed {result.job.seed}: atomic control lost {atomic} updates")
        lost[result.job.seed] = racy
    details = {"lost_by_seed": lost}
    if problems:
        return Verdict(FAIL, "; ".join(problems), details)
    if len(set(lost.values())) < 2:
        return Verdict(FAIL, f"all seeds gave the same outcome (lost={next(iter(lost.values()))})", details)
    failing = [seed for seed, n in lost.items() if n > 0]
    if not failing:
        return Verdict(FAIL, "no seed lost an update", details)
    seed = failing[0]
    original = results[pin["seeds"].index(seed)]
    (again,) = yield [Job(scenario, seed)]
    if (v := unresolved([again])):
        return v
    details["replayed_seed"] = seed
    diffs = witness_differences(original, again)
    if diffs or race_outcomes(again, machine)[0] != lost[seed]:
        return Verdict(FAIL, f"failing seed {seed} did not replay (differs in {', '.join(diffs) or 'outcome'})", details)
    return Verdict(PASS, f"{len(set(lost.values()))} distinct outcomes over {len(lost)} seeds; "
                   f"atomic control clean; seed {seed} (lost={lost[seed]}) replays", details)


def checker_control(ctx):
    """Item 4: a planted missing acknowledgement must be reported as RESULT fail, exit 1."""
    pin = ctx.pins["checker_control"]
    (result,) = yield [Job(pin["scenario"], pin["seed"])]
    if (v := unresolved([result])):
        return v
    details = {"result": result.result_line, "exit": result.workload_exit}
    if result.status == FAIL and result.workload_exit == 1 and result.result_line:
        return Verdict(PASS, f"expected failure detected: {result.result_line[:120]}", details)
    return Verdict(FAIL, f"checker missed the planted failure: status={result.status} "
                   f"exit={result.workload_exit} line={result.result_line!r}", details)


def known_failing(ctx):
    """Item 5: the pinned failing seed fails with the pinned witness in every fresh process."""
    pin = ctx.pins["known_failing"]
    scenario, seed, want = pin["scenario"], pin["seed"], pin["world_witness"]
    results = yield [default_job(scenario, seed)] + [Job(scenario, seed) for _ in range(pin["replicas"] - 1)]
    if (v := unresolved(results)):
        return v
    witnesses = {r.world_witness for r in results}
    outcomes = {r.status for r in results}
    details = {"witnesses": sorted(witnesses), "outcomes": sorted(outcomes), "pinned": want}
    if len(witnesses) > 1 or len(outcomes) > 1:
        return Verdict(FAIL, f"nondeterminism: {len(results)} fresh processes gave "
                       f"{len(witnesses)} witnesses and outcomes {sorted(outcomes)}", details)
    got = witnesses.pop()
    if got == want and outcomes == {FAIL}:
        return Verdict(PASS, f"{scenario} seed {seed}: RESULT fail, witness {want[:16]}… in {len(results)} fresh processes", details)
    current = ctx.identity(scenario) if ctx.identity else None
    pinned = pin.get("identity")
    details.update(current_identity=current, pinned_identity=pinned)
    changed = [k for k in sorted(set(pinned or {}) | set(current or {}))
               if (pinned or {}).get(k) != (current or {}).get(k)] if pinned else []
    if ctx.semantics_change or changed:
        why = "declared semantic change" if ctx.semantics_change else "changed inputs: " + ", ".join(changed)
        return Verdict(IDENTITY_CHANGED,
                       f"run identity changed ({why}); seed {seed} now gives {sorted(outcomes)} with witness "
                       f"{got[:16]}…. This is not a regression: run a fresh hunt and record its seed with "
                       f"`sim/acceptance.sh pin`.", details)
    if not pinned:
        return Verdict(UNRESOLVED, f"seed {seed} gave {sorted(outcomes)} with witness {got[:16]}…, pinned "
                       f"RESULT fail {want[:16]}…, and no input identity is pinned to tell a regression from "
                       f"a new run identity. Pass --semantics-change if the kernel/ABI or semantics changed.", details)
    return Verdict(FAIL, f"regression: same inputs, seed {seed} gave {sorted(outcomes)} with witness "
                   f"{got[:16]}…, pinned RESULT fail {want[:16]}…", details)


def fixed_control(ctx):
    """Item 6: the fixed version passes every seed in the declared range."""
    pin = ctx.pins["fixed_control"]
    first, last = pin["seeds"]
    results = yield [Job(pin["scenario"], seed) for seed in range(first, last + 1)]
    failed = [r.job.seed for r in results if r.status == FAIL]
    stuck = [r.job.seed for r in results if r.status == UNRESOLVED]
    unfaulted = [r.job.seed for r in results if r.completed and not r.faults]
    details = {"failed": failed, "unresolved": stuck, "unfaulted": unfaulted, "seeds": [first, last]}
    if unfaulted:
        return Verdict(FAIL, f"no kill/restart was applied for seeds {unfaulted}", details)
    if failed:
        return Verdict(FAIL, f"{len(failed)} failure(s) in fixed version: seeds {failed}", details)
    if stuck:
        return Verdict(UNRESOLVED, f"{len(stuck)} UNRESOLVED: seeds {stuck}", details)
    return Verdict(PASS, f"{pin['scenario']} seeds {first}-{last}: 0 failures, 0 UNRESOLVED", details)


def fault_effects(ctx):
    """Kill/restart faults were actually applied, and the application recovered."""
    pin = ctx.pins["fault_effects"]
    scenario, seed = pin["scenario"], pin["seed"]
    (result,) = yield [default_job(scenario, seed)]
    if (v := unresolved([result])):
        return v
    members = sorted({f.get("member") for f in result.faults})
    unrestarted = [f for f in result.faults if not f.get("start_ns", 0) > f.get("stop_ns", 0)]
    details = {"faults": result.faults, "result": result.result_line}
    if not result.faults:
        return Verdict(FAIL, f"{scenario} seed {seed}: no kill/restart was applied", details)
    if unrestarted:
        return Verdict(FAIL, f"{len(unrestarted)} killed member(s) were never restarted: {unrestarted}", details)
    if result.status != PASS:
        return Verdict(FAIL, f"application did not recover after {len(result.faults)} kill/restarts: "
                       f"{result.detail}", details)
    return Verdict(PASS, f"{scenario} seed {seed}: {len(result.faults)} kill/restarts of {', '.join(members)}, "
                   f"then {result.result_line[:80]}", details)


def cross_build(ctx):
    """Item 7: a declared performance-only change keeps every pinned witness."""
    if not ctx.performance_only:
        return Verdict(SKIPPED, "change not declared performance-only (use --performance-only OLD_VMM)")
    pins = [(ctx.pins[p]["scenario"], ctx.pins[p]["seed"]) for p in ctx.pins["cross_build"]]
    jobs = []
    for scenario, seed in pins:
        jobs += [default_job(scenario, seed), Job(scenario, seed, engine=BASELINE)]
    results = yield jobs
    if (v := unresolved(results)):
        return v
    details, failures = {}, []
    for new, old in zip(results[::2], results[1::2]):
        diffs = witness_differences(old, new)
        details[f"{new.job.scenario}/{new.job.seed}"] = diffs
        if diffs:
            failures.append(f"{new.job.scenario} seed {new.job.seed} differs in {', '.join(diffs)}")
    if failures:
        return Verdict(FAIL, "performance-only change altered results: " + "; ".join(failures), details)
    return Verdict(PASS, "baseline and candidate builds match on " +
                   ", ".join(f"{s}/{n}" for s, n in pins), details)


SMOKE = [("1 replay", replay), ("2 stop-placement invariance", stop_placement),
         ("3 race demo", race), ("4 checker negative control", checker_control),
         ("8 fault effects and recovery", fault_effects)]
FULL = SMOKE + [("5 known failing seed", known_failing), ("6 fixed-version control", fixed_control),
                ("7 cross-build equivalence", cross_build)]
