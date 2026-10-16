# Evaluation workloads

Small systems with planted bugs, for checking that Seedbox finds them and for
comparing exploration strategies. Each eval has a buggy `seedbox.yaml`, a
`seedbox.fixed.yaml` negative control, and Rust programs that report
properties through [`seedbox-sdk`](../crates/seedbox-sdk). Images build from
the repository root so they can use the SDK crate.

| Eval | Planted bug | Needs | Found (buggy) |
|---|---|---|---|
| [bank](bank) | A transfer releases its lock between reading and writing a balance (lost update). | Thread interleaving only | Every seed: a pipeline check, too easy to measure exploration |
| [kvlog](kvlog) | Writes are acknowledged before they are fsynced; a timer flushes every 50ms. | Crashes: `--fault-profile kvlog/crashes.yaml` | 1 of 10 seeds (100–109) on Apple Silicon |
| [replog](replog) | Any replica answers reads from its own copy; a follower cut off from the leader misses writes. | Leader–follower partitions: `--fault-profile replog/partitions.yaml` | 5 of 20 seeds (100–119); none without faults; the fixed variant (reads via the leader) never |
| [kvlog](kvlog) fixed | None: the negative control. | Flush errors and crashes: `--fault-profile kvlog/disk-errors.yaml` | Never; a store whose disk keeps failing stops, which the oracle reports as unverified rather than lost |
| [compact](compact) | Restart compaction empties the log before the new snapshot is installed. | A crash during the brief compaction after a restart: `--fault-profile compact/crashes.yaml` | 1 of 48 seeds (1000–1047); a sweep with gaps longer than the downtime never can |
| [casreg](casreg) | Compare-and-set releases its lock between comparing and setting. | Two concurrent compare-and-sets interleaved inside the window | 3 of 48 seeds (4000–4047); only the operation history shows it, through [`seedbox-check`](../crates/seedbox-check) |
| [retry](retry) | The table of applied request IDs lives only in memory, so a restarted log applies a retried append again. | A crash after an append is durable but before its acknowledgement, then the client choosing to retry: `--fault-profile retry/crashes.yaml` | 2 of 48 seeds (4000–4047); the fixed variant 0 of 24 |

Each new eval's checker was validated before its rate was measured: a
known failing schedule fails the buggy variant and passes the fixed one.
For compact, a typed trigger crashes the store on its `compacting` event
after an earlier crash; for casreg, `CASREG_YIELD=1` pauses between compare
and set (8 of 8 buggy runs violate, 0 of 8 fixed). `CASREG_WORK` widens the
race window (5000 rounds by default; 7000 gives 4 of 32 seeds). For retry,
`RETRY_ACK_DELAY_MS=300` holds each acknowledgement after the append is
durable and `RETRY_ALWAYS=1` makes the client always retry: 8 of 8 buggy runs
apply an append twice, 0 of 8 fixed. Retry's client takes its pacing and its
retry decision as engine choices (`seedbox_sdk::workload::Decisions`), so
seeds and branches decide them, and compact reports its recovery phase as SDK
state.

`seedbox-bench` compares strategies through the CLI on a running server,
censoring runs that find nothing at the budget:

```sh
cargo build --release -p seedbox-bench
target/release/seedbox-bench evals/compact --fault-profile evals/compact/crashes.yaml --budget 24
```

```sh
seedbox explore evals/kvlog --count 10 --fault-profile evals/kvlog/crashes.yaml
seedbox explore evals/kvlog/seedbox.fixed.yaml --count 10 --fault-profile evals/kvlog/crashes.yaml
```

A guided campaign at the same member budget, and the coverage variants
(`seedbox.coverage.yaml`) that also guide by blocks the servers reach:

```sh
seedbox campaign evals/kvlog --budget 16 --batch 4 --corpus 4 --fault-profile evals/kvlog/crashes.yaml
seedbox campaign evals/kvlog/seedbox.coverage.yaml --budget 16 --batch 4 --corpus 4 --fault-profile evals/kvlog/crashes.yaml
```

Campaign against seed sweep, 16 members each, seeds 200–600 (five runs per
row; a campaign's first generation is the sweep's first four seeds):

| Workload | Strategy | Failing members | Members to first failure |
|---|---|---|---|
| kvlog | campaign | 17 of 80 | 1, 8, 7, 13, 5 |
| kvlog | sweep | 12 of 80 | 1, 6, 7, 6, 5 |
| replog | campaign | 22 of 80 | 8, 1, 1, 3, 3 |
| replog | sweep | 21 of 80 | 6, 1, 1, 3, 3 |
| kvlog, replog fixed | campaign | 0 of 64 | censored at 16 |
| kvlog | campaign, coverage | 20 of 80 | 1, 8, 7, 13, 5 |
| replog | campaign, coverage | 22 of 80 | 11, 2, 1, 2, 1 |

Both bugs are within reach of a plain sweep at these seeds, so neither
workload yet separates the strategies; branching yields more failing
variants of a found bug rather than finding it sooner. Block coverage
saturates in the first member (about 1150 blocks on kvlog, 3470 on replog)
and later members add at most a handful, so it adds little guidance here;
coverage changes timing, so its rows are not paired with the others.

P2 planner (states, event-relative mutations, strategy portfolio, swarm
roots) against the P1b planner and a sweep, 24 members, batch 4, corpus 4,
seeds 200–600. Compact gained an SDK `phase` state between the two campaign
rows, which shifts timing, so its sweep was rerun with it:

| Workload | Strategy | Found | Failing members | Members to first failure |
|---|---|---|---|---|
| compact | P1b campaign | 4 of 5 | 5 of 120 | 14, 15, 18, 14, >24 |
| compact | P2 campaign | 4 of 5 | 15 of 120 | 6, >24, 12, 11, 22 |
| compact | sweep | 3 of 5 | 6 of 120 | 5, >24, 9, 8, >24 |
| compact fixed | P2 campaign | 0 of 5 | 0 of 120 | censored |
| casreg | P1b campaign | 3 of 5 | 4 of 120 | 2, >24, 14, >24, 21 |
| casreg | sweep | 3 of 5 | 4 of 120 | 2, >24, 14, >24, 21 |
| retry | P2 campaign | 3 of 5 | 6 of 120 | >24, 5, >24, 14, 8 |
| retry | sweep | 5 of 5 | 9 of 120 | 9, 13, 17, 5, 19 |

On compact, mutations (a crash placed after a parent's state change or
reboot) triple the failing members and halve the members to the first
failure against the P1b planner, and find the bug from parents that never
failed; a sweep still gets there first when a fresh seed happens to hit it.
Casreg runs finish before the first anchor, so its campaign is the sweep.
Retry needs a crash inside the sub-millisecond gap between durability and
acknowledgement; branching does not aim at that gap, and the sweep's extra
fresh roots do better. A branched finding exports as a cold rerun when that
matches: 60 MB for compact against 2.2 GB with its start checkpoint.

Discovery rates depend on the engine and architecture; remeasure after
changes rather than treating them as fixed.
