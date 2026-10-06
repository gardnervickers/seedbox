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
BASELINE = "baseline"


@dataclass
class Verdict:
    status: str
    summary: str
    details: dict = field(default_factory=dict)


@dataclass
class Context:
    pins: dict
    performance_only: bool = False


def default_job(scenario, seed):
    """The default-settings run of a sampled seed, shared between checks."""
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
    base, details, failures, unexercised = results[0], {}, [], []
    for result in results[1:]:
        label = " ".join(f"{k}={v}" for k, v in result.job.performance)
        diffs = witness_differences(base, result)
        details[label] = {"differs": diffs, "retries": result.retries}
        if diffs:
            failures.append(f"{label} differs in {', '.join(diffs)}")
        if result.retries and dict(result.job.performance).get("margin"):
            unexercised.append(label)
    if failures:
        return Verdict(FAIL, "determinism bug: " + "; ".join(failures), details)
    if unexercised:
        return Verdict(UNRESOLVED, f"{', '.join(unexercised)} not exercised: completed only after an "
                       "overshoot retry at a larger margin", details)
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


def fixed_control(ctx):
    """Item 6: the fixed version passes every seed in the declared range."""
    pin = ctx.pins["fixed_control"]
    first, last = pin["seeds"]
    results = yield [Job(pin["scenario"], seed) for seed in range(first, last + 1)]
    failed = [r.job.seed for r in results if r.status == FAIL]
    stuck = [r.job.seed for r in results if r.status == UNRESOLVED]
    unfaulted = [r.job.seed for r in results if r.completed and not restarted(r)]
    details = {"failed": failed, "unresolved": stuck, "unfaulted": unfaulted, "seeds": [first, last]}
    if unfaulted:
        return Verdict(FAIL, f"no acknowledged kill/restart for seeds {unfaulted}", details)
    if failed:
        return Verdict(FAIL, f"{len(failed)} failure(s) in fixed version: seeds {failed}", details)
    if stuck:
        return Verdict(UNRESOLVED, f"{len(stuck)} UNRESOLVED: seeds {stuck}", details)
    return Verdict(PASS, f"{pin['scenario']} seeds {first}-{last}: 0 failures, 0 UNRESOLVED", details)


def restarted(result):
    """Faults whose stop and restart the guest acknowledged."""
    return [f for f in result.faults if f["stopped"] and f["restarted"]]


def fault_effects(ctx):
    """Kill/restart faults were actually applied, and the application recovered."""
    pin = ctx.pins["fault_effects"]
    scenario, seed = pin["scenario"], pin["seed"]
    (result,) = yield [default_job(scenario, seed)]
    if (v := unresolved([result])):
        return v
    done = restarted(result)
    stopped = sum(f["stopped"] for f in result.faults)
    members = sorted({f["member"] for f in done})
    details = {"faults": result.faults, "result": result.result_line}
    if not done:
        return Verdict(FAIL, f"{scenario} seed {seed}: no guest acknowledged a kill and restart "
                       f"({len(result.faults)} scheduled, {stopped} stopped)", details)
    if result.status != PASS:
        return Verdict(FAIL, f"application did not recover after {len(done)} kill/restarts: "
                       f"{result.detail}", details)
    return Verdict(PASS, f"{scenario} seed {seed}: {len(done)} of {len(result.faults)} scheduled kill/restarts "
                   f"acknowledged ({', '.join(members)}), then {result.result_line[:80]}", details)


def cross_build(ctx):
    """Item 7: a declared performance-only change preserves fresh execution witnesses."""
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
FULL = SMOKE + [("6 fixed-version control", fixed_control),
                ("7 cross-build equivalence", cross_build)]
