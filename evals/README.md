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

```sh
seedbox explore evals/kvlog --count 10 --fault-profile evals/kvlog/crashes.yaml
seedbox explore evals/kvlog/seedbox.fixed.yaml --count 10 --fault-profile evals/kvlog/crashes.yaml
```

Discovery rates depend on the engine and architecture; remeasure after
changes rather than treating them as fixed.
