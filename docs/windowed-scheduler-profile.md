# Window4 host profile, 2026-10-08

Baseline: main at `cc2af345b53c775ec755530a6d79a8b533c0fd67` (#179).
Candidate changes only terminal per-page evidence hashing in
`crates/seedbox-vmm/src/machine.rs`. It does not change the window planner or
its semantic settings. No CLI, ARM, admission, RNG, transport or policy changes.
The same profile also measured the since-removed counter timebase; those rows
are omitted here.

## Before and after

Five interleaved trials per engine and configuration, seed 7,
`harness/worlds/window4.json`, `--digest`. Times are seconds. Positive savings
mean faster. These are unprofiled trials: perf runs are separate.

| Workers | Host before | Host after | Savings | Process before | Process after | Savings |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 3.343 | 3.336 | +0.2% | 4.509 | 4.091 | +9.3% |
| 4 | 0.949 | 0.956 | -0.7% | 2.137 | 1.727 | +19.2% |

`host_seconds` ends before the report computes RAM and per-page digests.
Process time includes those digests, evidence writes and teardown. Reusing the
zero-page digest therefore improves process time without improving scheduler
time. The scheduler changes are within the variation of these trials.

## Ranked costs

These are approximate seconds of aggregate CPU/work across all four machines,
including for four-worker profiles; they are not four-worker elapsed seconds.
Perf sampled `cpu-clock` at 997 Hz with DWARF stacks. Guest CPU is the guest-mode
sample share (shown as `[unknown]` by `perf kvm --host --guest report`). Host
hashing and page-fault costs are period-weighted inclusive stacks. Rows are
inclusive and should not be added together. Sampling costs are approximate to
about 10–20 ms.

Each cell is **before → after**. Ranked by the largest baseline serial cost.

| Rank | Cause | 1 worker | 4 workers |
| ---: | --- | ---: | ---: |
| 1 | Guest execution (hashing, boot and network work) | 3.221 → 3.218 | 3.274 → 3.275 |
| 2 | Complete RAM digest, after scheduler timing | 0.612 → 0.612 | 0.613 → 0.613 |
| 3 | Per-page digests, after scheduler timing | 0.474 → 0.053 | 0.475 → 0.055 |
| 4 | Other host page faults, including input loading and KVM RAM faults | 0.073 → 0.045 | 0.071 → 0.058 |
| 5 | Watchdog probes | 0.030 → 0.029 | 0.000 → 0.000 |
| 6 | Coordinator | 0.002 → 0.002 | 0.003 → 0.003 |

Runs mostly execute the guest: the baseline VMM records 3.38 s of machine
work, of which 3.33–3.35 s is inside KVM depending on worker count. A KVM entry
ending in PIO or EINTR includes the guest computation before the exit; this is
not the cost of handling that exit. There is a median one false watchdog probe
on one worker (4096 landings, about 29 ms) and none on four. No watchdog-policy
change is warranted. Migration/RCU and rayon scheduling were already measured
as small; this work did not repeat those investigations.

## How the fix preserves results

Page evidence reuses one SHA-256 digest only after comparing all bytes of a
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
  `cargo test-vmm --features kvm-tests` pass. The VMM is Linux-only; it was
  not built or tested on the Mac.
- The new pure page-hash test compares zero, changed, restored-zero and short
  pages byte-for-byte against ordinary SHA-256. The existing KVM page-format
  test also passes and checks that RAM and witness state are unchanged.
- All timed runs match main on outcome, world/network witnesses, committed
  end, machine times and state digests. Every `.pages`, `.console` and
  `.mgmt` file also matches byte-for-byte across engines and worker counts.
- Public CLI full-state run/replay reports `replay: MATCH` for both engines.
  This separate retained-input check runs the window workload through a
  temporary static init wrapper and four-service Compose envelope, because
  the low-level fixture is not a CLI Compose project. It is not a benchmark
  row or the same initramfs as the matrix. Four-worker replay uses the
  retained standalone engine and its actual `output/world.json`, without
  adding CLI worker flags.

## Setup and reproduction

AMD Ryzen 9 5950X, Linux 6.18.54, project `nix develop -c` on the workstation.
Builds and data lived in isolated worktrees; changes arrived via a patch.
Run, profile and replay data were created for this task and removed afterwards.
The original workstation checkout and retained engine/kernel stores were not
modified.

Kernel was built from baseline `kernel/`; initramfs used `guest/window.init`
and the project shell's static BusyBox. Measured byte identities:

- Kernel: `07f307403822600c00686cadd013c33adf1ff9fca66f54b9f2e75d5a15fa0c5a`.
- Fixture initramfs: `f6f859f10541b55acd1d56f23e582982dc0f2d4e12052c7c62eff8fc7fdb22e3`.
- Baseline VMM: `952e344ee9e4f663fc5ea42ab201540599b32e781b3464a979c89c0347bd9e30`.
- Candidate VMM: `2f811e23a659ef0fad4cdc734cf80d9a966714327c00926ddf2d4e60cae41ee6`.

Replace only the fixture's kernel/initrd paths. Keep seed 7, `--digest`, memory
256 MiB, work 20000, network latency 1 ms/jitter 100 us and stop tolerance 2 ms.
Invocation shapes:

```sh
# One worker: use this affinity mask for the complete process.
taskset -c 14,15,30,31 "$VMM" --config "$WORLD" --seed 7 --out "$OUT" --digest
# Four workers: pin to four physical cores, with the coordinator in this mask.
taskset -c 12-15,28-31 "$VMM" --config "$WORLD" --seed 7 --out "$OUT" --digest --machine-workers 4 --machine-cpus 12,13,14,15
# Profile separately from timing.
perf record -e cpu-clock -F 997 --call-graph dwarf,16384 -o "$PERF_DATA" -- taskset -c 14,15,30,31 "$VMM" --config "$WORLD" --seed 7 --out "$OUT" --digest
perf kvm --host --guest report --stdio --no-children -g none --sort dso -i "$PERF_DATA"
perf script -i "$PERF_DATA" -F period,ip,sym,dso
```

Use `harness/windowed-matrix.sh` for the broader existing agreement matrix.
For the timing experiment, run a fresh process/output directory for every trial.
Alternate the worker-count order between trials; within each configuration run
baseline/candidate on odd trials and candidate/baseline on even trials. No perf
collection runs during timing.

## Five interleaved trials

Seconds, in trial order; unrounded measurements were used for the medians.

| Workers | Trial | Host before | Host after | Process before | Process after |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 3.301264 | 3.336293 | 4.496513 | 4.090844 |
| 1 | 2 | 3.342614 | 3.297084 | 4.508516 | 4.048398 |
| 1 | 3 | 3.348807 | 3.300413 | 4.530079 | 4.044977 |
| 1 | 4 | 3.298016 | 3.342531 | 4.472547 | 4.116782 |
| 1 | 5 | 3.479683 | 3.403294 | 4.693608 | 4.178965 |
| 4 | 1 | 0.949259 | 1.007350 | 2.129428 | 1.785145 |
| 4 | 2 | 0.934314 | 0.956124 | 2.136544 | 1.709374 |
| 4 | 3 | 0.926187 | 0.926756 | 2.115946 | 1.706310 |
| 4 | 4 | 1.007681 | 0.950324 | 2.194430 | 1.726572 |
| 4 | 5 | 0.993583 | 0.983051 | 2.230736 | 1.833986 |

## Larger opportunities left for later

- Parallel terminal RAM digests could ideally reduce about 0.61 s of serial
  work to 0.15 s at four workers, saving at most roughly 0.46 s of process
  elapsed time. It needs stopped-state ownership and ordered report collection;
  this change leaves that larger report restructuring for later.
- The dominant cost is executing the fixture's hashing workload. Faster guest
  code would change retained guest inputs, and is not a host scheduler fix.

This is fixture evidence on one host, not the full engine merge train.
Application host acceptance and final merge-train checks remain separate.
