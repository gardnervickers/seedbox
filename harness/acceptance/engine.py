"""Drives checks round by round and combines their jobs into shared launches."""
from dataclasses import dataclass, field
import traceback

from .checks import FAIL, SKIPPED, UNRESOLVED, Verdict
from .model import RunResult


@dataclass
class Outcome:
    name: str
    verdict: Verdict = None
    runs: list = field(default_factory=list)

    @property
    def host_seconds(self):
        return round(sum(r.host_seconds or 0 for r in self.runs), 1)


def execute(checks, ctx, launcher):
    """Run every check to a verdict. A launcher maps a job list to a result list."""
    outcomes, active, cache = [], [], {}

    def advance(outcome, gen, results):
        try:
            jobs = gen.send(results)
        except StopIteration as stop:
            outcome.verdict = stop.value
        except Exception:
            outcome.verdict = Verdict(UNRESOLVED, f"check error: {traceback.format_exc(limit=2)}")
        else:
            active.append((outcome, gen, jobs))

    for name, check in checks:
        outcome = Outcome(name)
        outcomes.append(outcome)
        advance(outcome, check(ctx), None)
    while active:
        pending, active[:] = list(active), []
        unique, refs = [], []
        for _, _, jobs in pending:
            row = []
            for job in jobs:
                if job.share and job.share in cache:
                    row.append(cache[job.share])
                    continue
                if job.share:
                    cache[job.share] = len(unique)
                row.append(len(unique))
                unique.append(job)
            refs.append(row)
        try:
            results = launcher.run(unique) if unique else []
        except Exception:
            error = traceback.format_exc(limit=3)
            results = [RunResult(job, UNRESOLVED, detail=f"launcher error: {error}") for job in unique]
        for job, result in zip(unique, results):
            if job.share and isinstance(cache.get(job.share), int):
                cache[job.share] = result
        for (outcome, gen, _), row in zip(pending, refs):
            mine = [ref if not isinstance(ref, int) else results[ref] for ref in row]
            outcome.runs += mine
            advance(outcome, gen, mine)
    return outcomes


# 1 is a regression only; argparse uses 2 for usage errors.
EXIT_CODES = {"PASS": 0, "REGRESSION": 1, "UNRESOLVED": 3}


def overall(outcomes):
    statuses = {o.verdict.status for o in outcomes}
    if FAIL in statuses:
        return "REGRESSION"
    if UNRESOLVED in statuses:
        return "UNRESOLVED"
    return "PASS"


def render(outcomes):
    lines = []
    for o in outcomes:
        mark = {"pass": "ok", SKIPPED: "--"}.get(o.verdict.status, "!!")
        lines.append(f"{mark} [{o.verdict.status.upper()}] {o.name}: {o.verdict.summary}")
        if o.runs:
            lines.append(f"     {len(o.runs)} run(s), {o.host_seconds} host s")
    return "\n".join(lines)
