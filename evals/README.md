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

Discovery rates depend on the engine and architecture; remeasure after
changes rather than treating them as fixed.
