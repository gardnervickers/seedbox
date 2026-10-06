"""Runner-neutral vocabulary shared by checks and launchers.

Checks describe runs as Jobs and judge RunResults. The CLI launcher maps them
to YAML scenarios and fresh executions or replays of retained inputs.
"""
from dataclasses import dataclass, field

EXITS = "exits"            # exit witnesses only
FULL_STATE = "full_state"  # exit witnesses plus RAM/register digests

PASS, FAIL, UNRESOLVED = "pass", "fail", "unresolved"


@dataclass(frozen=True)
class Job:
    """One fresh execution of a scenario. Equal jobs are still separate runs."""
    scenario: str
    seed: int
    witness: str = EXITS
    # Settings that must not change results, as sorted (name, value) pairs.
    performance: tuple = ()
    # None runs the build under test; otherwise a named alternative build.
    engine: str | None = None
    # Jobs with the same non-empty key run once and share the result.
    share: str = ""

    def describe(self):
        parts = [f"{self.scenario} seed={self.seed}"]
        if self.witness != EXITS:
            parts.append(self.witness)
        parts += [f"{k}={v}" for k, v in self.performance]
        if self.engine:
            parts.append(f"engine={self.engine}")
        return " ".join(parts)


def performance(**settings):
    return tuple(sorted((k, str(v)) for k, v in settings.items()))


@dataclass
class RunResult:
    job: Job
    # PASS/FAIL when the workload reported a checker RESULT, PASS/FAIL by exit
    # status when it reports none, UNRESOLVED when the run did not complete.
    status: str
    detail: str = ""
    result_line: str | None = None
    workload_exit: int | None = None
    world_witness: str | None = None
    machine_witnesses: dict = field(default_factory=dict)
    state_digests: dict = field(default_factory=dict)
    sim_seconds: float | None = None
    host_seconds: float | None = None
    exits: dict = field(default_factory=dict)
    retries: int = 0
    evidence: str = ""
    logs: dict = field(default_factory=dict)  # machine -> console text
    # Confirmed effects from the public execution report.
    faults: list = field(default_factory=list)
    replay_matches: bool = True
    network_counters: dict = field(default_factory=dict)
    # Earlier attempts of this job that did not count: [{evidence, detail}].
    attempts: list = field(default_factory=list)

    @property
    def completed(self):
        return self.status != UNRESOLVED

    def witness_key(self):
        """Everything a deterministic replay must reproduce."""
        return (self.world_witness, tuple(sorted(self.machine_witnesses.items())),
                tuple(sorted(self.state_digests.items())))

    def summary(self):
        return {
            "job": self.job.describe(),
            "status": self.status,
            "detail": self.detail,
            "world_witness": self.world_witness,
            "machine_witnesses": self.machine_witnesses,
            "state_digests": self.state_digests,
            "sim_seconds": self.sim_seconds,
            "host_seconds": self.host_seconds,
            "exits": self.exits,
            "retries": self.retries,
            "faults": self.faults,
            "replay_matches": self.replay_matches,
            "network_counters": self.network_counters,
            "evidence": self.evidence,
            "failed_attempts": self.attempts,
        }


def witness_differences(a, b):
    """Names of the witnesses that differ between two completed runs."""
    diffs = []
    if a.world_witness != b.world_witness:
        diffs.append("world")
    for name in sorted(set(a.machine_witnesses) | set(b.machine_witnesses)):
        if a.machine_witnesses.get(name) != b.machine_witnesses.get(name):
            diffs.append(name)
    for name in sorted(set(a.state_digests) | set(b.state_digests)):
        if a.state_digests.get(name) != b.state_digests.get(name):
            diffs.append(f"{name}:digest")
    if a.faults != b.faults:
        diffs.append("fault effects")
    if not a.replay_matches or not b.replay_matches:
        diffs.append("CLI replay comparison")
    return diffs
