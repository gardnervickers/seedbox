# process-churn

Probes the translator boundary: DynamoRIO's follow-children re-injection on
`execve`, per-thread block-budget inheritance across `fork` and `vfork`,
code-cache flushes on self-modifying code, signal delivery to a spinning
thread, and (on ARM) the rewritten LL/SC sequences and wait hints. Virtual time
per phase also exposes a process that escaped the translator: its compute
would cost nothing.

One service, built from `Dockerfile`: a static C program (`churn.c`) on
`alpine:3.22` (not scratch; phase a needs busybox `sh`, `seq`, `sort`, `uniq`,
`wc`, and the digest uses `sha256sum`). Phases, in order, each reporting its
`CLOCK_MONOTONIC` virtual duration as `<phase>_ns`:

| Phase | Work | Other lines |
| --- | --- | --- |
| a | 30 `fork`+`execve /bin/sh -c "seq 1 2000 \| sort -r \| uniq -c \| wc -l"` | `pipeline_ok` (output checked) |
| b | 30 `posix_spawn /bin/true` (musl uses `CLONE_VFORK`) + `waitpid` | `spawn_ok` |
| c | 20 `fork`s; each child runs a 50,000-iteration loop and pipes its own loop time back | `fork_child_loop_ns` (all 20), `_sum` |
| d | create then join 200 threads, 2,000 iterations each | `threads_ok`, `threads_sum` |
| e | producer/consumer over a single-slot mailbox, weak `__atomic_compare_exchange_n` + `sched_yield` | `mailbox_cas_fail`, `_cas_spurious`, `_yields` (producer,consumer), `_out_of_order` |
| f | two threads, 20,000 `pthread_spin_lock` sections each | `spin_acquisitions`, `spin_owner_switches`, `spin_owner_digest` (FNV of the owner sequence) |
| g | JIT: anonymous page RW, write `mov eax, imm32; ret` (x86-64) or `movz w0, #imm; ret` (aarch64), RX, call; 101 rewrites | `jit_calls`, `jit_mismatches`; `jit: unsupported` on other arches |
| h | a thread sends 500 `SIGUSR1` to the main thread, which spins then `nanosleep`s (no `SA_RESTART`) | `signal_handled`, `signal_eintr`, `signal_spin_rounds` |

The last line is `digest: <sha256>` over every line printed before it
(computed by piping them to busybox `sha256sum`). All lines are the trace;
there is no separate event log.

## Comparing runs

```sh
./build/current/seedbox run examples/determinism/process-churn --watchdog-ms 1000
./build/current/seedbox run examples/determinism/process-churn --watchdog-ms 1
./build/current/seedbox diff .seedbox/runs/A .seedbox/runs/B
```

Control knob: `--watchdog-ms` (1 versus 1000). It is a performance setting
and must not change any console byte. Same seed (1105) must print identical
output; a differing `*_ns` line localizes the phase, and a `fork_child_loop_ns`
or `*_ns` near zero means a process ran untranslated.
Expected invariants: every `_ok` is full, `mailbox_out_of_order: 0`,
`spin_acquisitions: 40000`, `jit_mismatches: 0`. The counts in phases e, f
and h are scheduling-dependent by design; they must only be repeatable.
