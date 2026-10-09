# Window4 host profile, 2026-10-08

Baseline: main at `cc2af345b53c775ec755530a6d79a8b533c0fd67` (#179).
Candidate changes only counter refinement and terminal per-page evidence hashing
in `crates/seedbox-vmm/src/machine/counted.rs` and
`crates/seedbox-vmm/src/machine.rs`. Neither changes the window planner or its
semantic settings. No CLI, ARM, admission, RNG, transport or policy changes.

## Before and after

Five interleaved trials per engine and configuration, seed 7,
`harness/worlds/window4.json`, `--digest`. Times are seconds. Positive savings
mean faster. These are unprofiled trials: perf runs are separate.

| Timebase | Workers | Host before | Host after | Savings | Process before | Process after | Savings |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| counter | 1 | 4.582 | 4.371 | +4.6% | 5.779 | 5.155 | +10.8% |
| counter | 4 | 1.303 | 1.295 | +0.6% | 2.510 | 2.062 | +17.8% |
| pv | 1 | 3.343 | 3.336 | +0.2% | 4.509 | 4.091 | +9.3% |
| pv | 4 | 0.949 | 0.956 | -0.7% | 2.137 | 1.727 | +19.2% |

`host_seconds` ends before the report computes RAM and per-page digests.
Process time includes those digests, evidence writes and teardown. Reusing the
zero-page digest therefore improves process time without improving scheduler
time. Counter serial scheduler time improves 4.6%; the counter four-worker and
PV scheduler changes are within the variation of these trials. The process
improvements are 10.8%, 17.8%, 9.3% and 19.2%, respectively.

## Ranked costs

These are approximate seconds of aggregate CPU/work across all four machines,
including for four-worker profiles; they are not four-worker elapsed seconds.
Perf sampled `cpu-clock` at 997 Hz with DWARF stacks. Guest CPU is the guest-mode
sample share (shown as `[unknown]` by `perf kvm --host --guest report`). Host
hashing and page-fault costs are period-weighted inclusive stacks. Counter
stepping and checkpoint/rollback costs use medians of the VMM's own statistics.
Stepping includes its KVM entries and planning; rows are inclusive and should
not be added together. Sampling costs are approximate to about 10–20 ms.

Each cell is **before → after**. Ranked by the largest baseline serial cost.

| Rank | Cause | Counter, 1 | Counter, 4 | PV, 1 | PV, 4 |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | Guest execution (hashing, boot and network work) | 3.851 → 3.658 | 3.763 → 3.683 | 3.221 → 3.218 | 3.274 → 3.275 |
| 2 | Counter precision stops / PV watchdog probes | 0.744 → 0.476 | 0.750 → 0.484 | 0.030 → 0.029 | 0.000 → 0.000 |
| 3 | Complete RAM digest, after scheduler timing | 0.612 → 0.610 | 0.614 → 0.612 | 0.612 → 0.612 | 0.613 → 0.613 |
| 4 | Per-page digests, after scheduler timing | 0.472 → 0.053 | 0.475 → 0.053 | 0.474 → 0.053 | 0.475 → 0.055 |
| 5 | Counter checkpoints + rollbacks | 0.140 → 0.168 | 0.190 → 0.215 | 0.000 → 0.000 | 0.000 → 0.000 |
| 6 | Other host page faults, including input loading and KVM RAM faults | 0.080 → 0.083 | 0.078 → 0.093 | 0.073 → 0.045 | 0.071 → 0.058 |
| 7 | Coordinator | 0.002 → 0.002 | 0.002 → 0.002 | 0.002 → 0.002 | 0.003 → 0.003 |

PV runs mostly execute the guest: the baseline VMM records 3.38 s of machine
work, of which 3.33–3.35 s is inside KVM depending on worker count. A KVM entry
ending in PIO or EINTR includes the guest computation before the exit; this is
not the cost of handling that exit. PV has a median one false watchdog probe
on one worker (4096 landings, about 29 ms) and none on four. No watchdog-policy
change is warranted. Counter ioctl, migration/RCU and rayon scheduling were
already measured as small; this work did not repeat those investigations.

## How the fixes preserve results

- Counter refinement now accepts an unobservable overflow landing with more
  than 32 branches left, rather than 128. It still requires a valid undo
  checkpoint and strictly later machine time before taking another checkpoint.
  The hard target, quarter-distance margin, rollback growth, legal-exit rules
  and RNG use are unchanged. At the held checkpoint after a rollback the guard
  is false, so it cannot continually restart a failed refinement. `--undo 0`
  and PV still bypass this path. Median serial steps fall from 105,141 to
  69,821; four-worker totals fall from 104,572 to 68,692. Additional serial
  checkpoint/rollback work costs about 28 ms.
- Page evidence reuses one SHA-256 digest only after comparing all bytes of a
  full 4096-byte page with zero. Nonzero pages and a short trailing page retain
  ordinary hashing. Output is the same first eight digest bytes in PFN order,
  and output capacity is reserved once. No dirty-log inference or retained
  cache is involved. The complete RAM digest remains unchanged. The page pass
  falls from about 474 ms to 53 ms.

Existing rules retained: exact stop states, RNG streams/draw order, event and
fault ordering, lifecycle and admission/validation, device dirty journals,
feature-off witnesses, CLI output, retained-engine replay and other agents'
CLI/ARM work. All performance settings remain excluded from semantic results.

## Validation

- `cargo fmt -p seedbox --check`,
  `cargo clippy -p seedbox --all-targets --locked -- -D warnings`, and
  `cargo test --workspace --locked` pass on the Mac. The portable gate of the
  time (since replaced by `bash harness/check.sh portable`) also passes,
  including 161 harness Python tests and 17 API Python tests.
- Linux `cargo build-vmm`, `cargo test-vmm` and
  `cargo test-vmm --features kvm-tests` pass. The KVM suite reports 90 passed,
  37 ignored. The VMM is Linux-only; it was not built or tested on the Mac.
- The new pure page-hash test compares zero, changed, restored-zero and short
  pages byte-for-byte against ordinary SHA-256. The existing KVM page-format
  test also passes and checks that RAM and witness state are unchanged.
- Explicit AMD/MSR runs of
  `refined_counter_approaches_preserve_ram_and_exact_stops` and
  `stops_at_natural_exits_or_exact_hard_counts_without_yields` pass. The first
  now also covers undo margins 32, 64 and 128, in addition to 0, 16, 256, 1024
  and 4096; its guest changes RAM on every branch.
- All 40 timed runs match main for their timebase on outcome, world/network
  witnesses, committed end, machine times and state digests. Every `.pages`,
  `.console` and `.mgmt` file also matches byte-for-byte across engines and
  worker counts.
- Public CLI full-state run/replay reports `replay: MATCH` for both engines on
  both timebases. This separate retained-input check runs the window workload
  through a temporary static init wrapper and four-service Compose envelope,
  because the low-level fixture is not a CLI Compose project. It is not a
  benchmark row or the same initramfs as the matrix. Four-worker replay uses
  the retained standalone engine and its actual `output/world.json`, without
  adding CLI worker flags.

## Setup and reproduction

AMD Ryzen 9 5950X, Linux 6.18.54, project `nix develop -c` on the workstation.
Builds and data lived in isolated worktrees; changes arrived via a patch.
Run, profile and replay data were created for this task and removed afterwards.
The original workstation checkout and retained engine/kernel stores were not
modified. The missing workstation `robo-git` helper was avoided by transferring
the baseline as an offline Git bundle, without GitHub authentication.

Kernel was built from baseline `kernel/`; initramfs used `guest/window.init`
and the project shell's static BusyBox. Measured byte identities:

- Kernel: `07f307403822600c00686cadd013c33adf1ff9fca66f54b9f2e75d5a15fa0c5a`.
- Fixture initramfs: `f6f859f10541b55acd1d56f23e582982dc0f2d4e12052c7c62eff8fc7fdb22e3`.
- Baseline VMM: `952e344ee9e4f663fc5ea42ab201540599b32e781b3464a979c89c0347bd9e30`.
- Candidate VMM: `2f811e23a659ef0fad4cdc734cf80d9a966714327c00926ddf2d4e60cae41ee6`.

Replace only the fixture's kernel/initrd paths. Keep seed 7, `--digest`, memory
256 MiB, work 20000, network latency 1 ms/jitter 100 us and stop tolerance 2 ms.
Apply the scoped Zen MSR workaround on CPUs 12–15 and their SMT siblings 28–31,
and restore the saved values after each group of runs. Invocation shapes:

```sh
# One worker: use this affinity mask for the complete process.
taskset -c 14,15,30,31 "$VMM" --config "$WORLD" --seed 7 --out "$OUT" --digest --timebase "$TIMEBASE"
# Four workers: pin to four physical cores, with the coordinator in this mask.
taskset -c 12-15,28-31 "$VMM" --config "$WORLD" --seed 7 --out "$OUT" --digest --timebase "$TIMEBASE" --machine-workers 4 --machine-cpus 12,13,14,15
# Profile separately from timing. The VMM's branch counter uses no perf sampling PMU.
perf record -e cpu-clock -F 997 --call-graph dwarf,16384 -o "$PERF_DATA" -- taskset -c 14,15,30,31 "$VMM" --config "$WORLD" --seed 7 --out "$OUT" --digest --timebase "$TIMEBASE"
perf kvm --host --guest report --stdio --no-children -g none --sort dso -i "$PERF_DATA"
perf script -i "$PERF_DATA" -F period,ip,sym,dso
```

Use `harness/windowed-matrix.sh` for the broader existing agreement matrix.
For the timing experiment, run a fresh process/output directory for every trial.
Trial order is counter/1, PV/4, PV/1, counter/4 on odd trials and the reverse on
even trials; within each configuration run baseline/candidate on odd trials and
candidate/baseline on even trials. No perf collection runs during timing.

## Five interleaved trials

Seconds, in trial order; unrounded measurements were used for the medians.

| Timebase | Workers | Trial | Host before | Host after | Process before | Process after |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| counter | 1 | 1 | 4.549529 | 4.360957 | 5.761344 | 5.128145 |
| counter | 1 | 2 | 4.581982 | 4.367982 | 5.779059 | 5.155261 |
| counter | 1 | 3 | 4.565758 | 4.371410 | 5.763732 | 5.145277 |
| counter | 1 | 4 | 5.022855 | 4.396863 | 6.304622 | 5.224217 |
| counter | 1 | 5 | 4.767414 | 4.640598 | 6.049527 | 5.444324 |
| counter | 4 | 1 | 1.290715 | 1.301738 | 2.493269 | 2.062104 |
| counter | 4 | 2 | 1.302809 | 1.295044 | 2.510340 | 2.040782 |
| counter | 4 | 3 | 1.372378 | 1.274191 | 2.588365 | 2.082787 |
| counter | 4 | 4 | 1.303023 | 1.307074 | 2.510102 | 2.089808 |
| counter | 4 | 5 | 1.309801 | 1.247155 | 2.505866 | 2.035919 |
| pv | 1 | 1 | 3.301264 | 3.336293 | 4.496513 | 4.090844 |
| pv | 1 | 2 | 3.342614 | 3.297084 | 4.508516 | 4.048398 |
| pv | 1 | 3 | 3.348807 | 3.300413 | 4.530079 | 4.044977 |
| pv | 1 | 4 | 3.298016 | 3.342531 | 4.472547 | 4.116782 |
| pv | 1 | 5 | 3.479683 | 3.403294 | 4.693608 | 4.178965 |
| pv | 4 | 1 | 0.949259 | 1.007350 | 2.129428 | 1.785145 |
| pv | 4 | 2 | 0.934314 | 0.956124 | 2.136544 | 1.709374 |
| pv | 4 | 3 | 0.926187 | 0.926756 | 2.115946 | 1.706310 |
| pv | 4 | 4 | 1.007681 | 0.950324 | 2.194430 | 1.726572 |
| pv | 4 | 5 | 0.993583 | 0.983051 | 2.230736 | 1.833986 |

## Larger opportunities left for later

- Parallel terminal RAM digests could ideally reduce about 0.61 s of serial
  work to 0.15 s at four workers, saving at most roughly 0.46 s of process
  elapsed time. It needs stopped-state ownership and ordered report collection;
  this change leaves that larger report restructuring for later.
- Counter still pays about 0.48 s for precise stops. A better bounded approach
  or safe batching has that as a serial upper bound (about 0.12 s with perfect
  four-way overlap), before its own checkpoint/rollback costs. Changing window
  widths or allowing later/softer stops would change results and is outside this
  fix.
- PV's dominant cost is executing the fixture's hashing workload. Faster guest
  code would change retained guest inputs, and is not a host scheduler fix.

This is fixture evidence on the AMD host, not the full engine merge train.
Application host acceptance and final merge-train checks remain separate.
