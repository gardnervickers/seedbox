# Determinism counterexample probes

Small applications written the way an ordinary service would be, each chosen
because it leans on one mechanism seedbox uses to keep the same seed giving the
same result. Each prints a digest of a fine-grained event trace, so any
perturbation of that mechanism becomes a different console line. They are
optional runs, outside the acceptance suite and GitHub CI.

| Directory | Probes | Control knob that must not change the result |
| --- | --- | --- |
| `timer-races` | Timer interrupt landing: timeouts racing events armed for the same virtual instant | `--watchdog-ms` |
| `cpu-time` | Tick-based CPU accounting, scheduler statistics, `SIGPROF` | `--watchdog-ms` |
| `gossip` | Windowed scheduler causal rounds, output ordering and TX pickup with sub-latency traffic | `--machine-workers` |
| `randomness` | Guest CRNG reseeds from interrupt entropy, ASLR, ephemeral ports over 70 s | none (long run) |
| `process-churn` | Translator boundaries: follow-children, fork mid-budget, self-modifying code, signals, spin-waits | `--watchdog-ms` |

Checking one probe:

```sh
./build/current/seedbox run examples/determinism/timer-races --full-state
./build/current/seedbox run examples/determinism/timer-races --full-state
./build/current/seedbox diff .seedbox/runs/A .seedbox/runs/B
./build/current/seedbox replay .seedbox/runs/A
./build/current/seedbox run examples/determinism/timer-races --full-state --watchdog-ms 1
```

Compare the `digest:` lines in `output/machines/*.console` across the runs as
well as the `diff` verdict: the digest is the app-level result. The reasoning
behind the selection is in the project note `determinism-counterexamples/cases.md`.
