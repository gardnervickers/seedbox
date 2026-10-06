# Local Mac TCG input-control probe

A bounded research fixture for [issue #8](https://github.com/gardnervickers/seedbox/issues/8).
It boots real x86-64 Linux locally on an Apple Silicon Mac using QEMU software
emulation. Guest compilation also runs locally using Zig's Linux/musl target.
It does not change the production KVM runner. The subsequent
[service integration report](SERVICE_REPORT.md) reuses #10's merged fixture
and records fault/recovery qualification and a scoped execution witness.
The measurements below remain the earlier standalone input-control probe.

## Measured result, 2026-10-06

On an Apple M4 Max (Mac16,6), macOS 27.0.1 / 26A434, QEMU 11.1.1:

| Check | Result |
| --- | --- |
| Seed 42, five fresh QEMU processes | Identical complete console and ordered probe observations |
| Seeds 43 and 44, two fresh processes each | Each repeats exactly; payload entropy differs across all three seeds |
| Successful fresh-process suite | 9/9; median boot-to-exit 2.992 s, range 2.977–3.156 s |
| Omit Linux entropy injection | Explicit `FAIL: entropy_not_ready` |
| Plant a workload failure | Explicit `FAIL: planted_failure` despite QEMU exiting zero |
| Omit instruction counting | Two successful same-seed runs have different observations |
| Force a 10 ms host limit | `TIMEOUT`, guest process group terminated |
| Rebuild locally and rerun seed 42 | Same observation hash as the five-run suite |

All 16 launches, including pilot and controls, consumed 43.98 seconds of measured
QEMU wall time. The guest has 256 MiB configured RAM. The nine-run harness's
child-process peak RSS was 224,657,408 bytes (~214 MiB), as reported by macOS
`getrusage`; this is a batch child-process maximum, not guest RAM usage or a
whole-system memory measurement. Nix dependency download and compilation time
are outside the boot-to-exit measurement.

[Machine-readable checks](evidence/2026-10-06/summary.json) and the adjacent
per-batch directories retain exact argv, input/firmware hashes, source snapshots,
console bytes, structured observations and errors. Larger binary inputs remain
in ignored `build/` and `runs/` bundles. This is **observable repeatability of a
small input-control probe**, not a full execution witness, full-state comparison,
service acceptance or proven backend determinism. The positive suite exercises
no network devices, service concurrency, faults, durable disks or snapshots.

## Reproduce

Use this directory's flake, pinned to the project's existing nixpkgs revision
`8c6b39505383e6d5dcb431f00cb3597b71f8045c`. Commands below start here:

```sh
nix develop --command bash build-guest.sh
nix develop --command python3 -m unittest discover -p test_run.py -v
nix develop --command python3 run.py \
  --kernel build/bzImage --output runs/new-repeats
```

`build/bzImage` must be the unmodified stock Linux 6.18.54 kernel with SHA256
`66111c2888091bd64e96e71a6a9e831ab5496d6c6272add5d81490396bfa3cdc`.
The measured artifact was copied read-only from the earlier QEMU experiment's
Nix output, `/nix/store/wj7k7ng3vb1zfhs4k4wbv1l1sixk292f-linux-6.18.54/bzImage`.
The runner rejects a different kernel. Fresh-checkout kernel acquisition is
still a preparation dependency; no automated download/build is claimed here.
The current worktree already has it. Do not substitute the KVM-patched kernel:
stock QEMU does not implement its private time-page/port ABI.

The runner uses a new output directory and keeps its own kernel, executable,
initramfs and source copies. Once those inputs and the pinned Nix/QEMU closure
are cached, compilation and execution require no network or Linux builder.
Binary reproducibility across different build hosts has not been established.
An offline rerun can use `--kernel runs/<previous>/kernel --probe
runs/<previous>/probe` with the matching source/profile; this is a research
runner, not #2's eventual `seedbox replay` implementation.

Controls use separate directories:

```sh
nix develop --command python3 run.py --kernel build/bzImage \
  --output runs/new-no-entropy --seeds 42 --control no-entropy
nix develop --command python3 run.py --kernel build/bzImage \
  --output runs/new-planted --seeds 42 --control plant
nix develop --command python3 run.py --kernel build/bzImage \
  --output runs/new-plain --seeds 42 42 --plain-tcg
nix develop --command python3 run.py --kernel build/bzImage \
  --output runs/new-timeout --seeds 42 --timeout 0.01
```

Expected negative application controls return success from the *experiment
harness* only when their declared failure appears. Their run records remain
`FAIL`. Ordinary TCG permits observed divergence for the negative comparison;
its success exit code does not establish repeatability. The forced-timeout
command returns nonzero. Default bounds are 60 seconds/run and 600 seconds/batch;
the tool rejects bounds above 120 seconds/run and 600 seconds/batch. It stops on
an unexpected outcome and retains completed attempts plus the failing attempt.

## Inputs and observations

- One vCPU, `pc-i440fx-11.1,hpet=off`, `qemu64` with RDRAND/RDSEED disabled,
  no default devices, network, disks, monitor or interactive serial input.
- Fixed `-icount shift=3,align=off,sleep=off`: 8 virtual ns per instruction,
  idle jumps to the next timer, no wall-time synchronization. RTC has a fixed
  epoch and uses VM time. This is a simulation clock, not native CPU calibration.
- Domain-separated SHA256-derived streams supply QEMU's `-seed` and 32 bytes
  packaged in a canonical initramfs. PID1 credits those bytes with
  `RNDADDENTROPY`, then execs the probe so its `AT_RANDOM` is allocated after
  initialization. This deliberately predictable entropy belongs only in the
  simulation profile. The initial PID1's pre-seed `AT_RANDOM` is observed too.
- The payload checks nonblocking and blocking `getrandom`, CPU RNG feature
  masking, libc monotonic/realtime clocks, direct RDTSC, CPU-only work, three
  nanosleep deadlines and bounded clock polling. Seeded bytes affect its work
  hash. libc/vDSO behavior is part of the pinned Linux/musl guest input.
- SHA256 covers the ordered observations and complete serial output separately.
  These hashes must not be named execution witnesses or compared with KVM
  witnesses. No QEMU recording or replay log is used.

Relevant QEMU mechanisms: [instruction counting](https://www.qemu.org/docs/master/devel/tcg-icount.html)
and [invocation controls](https://www.qemu.org/docs/master/system/invocation.html).
The retained argv and QEMU binary hashes, rather than moving master documentation,
identify what was executed.

## Minimal integration contract for the next slice

Keep this as a research adapter until #2's run API is ready. #10's merged
fixture is now integrated in `service.py`; see the service report above.
Do not publish another competing Compose or manifest schema.

| Boundary | Required information / behavior |
| --- | --- |
| Prepared run in | Immutable guest/service inputs, seed, backend and topology profile, virtual-time policy, required capabilities and wall/simulation limits |
| Guest bootstrap | Inject entropy before launching services, deliver preloaded scenario inputs, and identify the guest/control ABI |
| Controlled observations out | Correlated applied/failed scenario actions with simulated timestamps; application completion and reason; console and declared witness scope |
| Result out | PASS/FAIL/ERROR/TIMEOUT/INCOMPLETE, retained inputs and logs, requested/applied fault history, host costs and profile identity |

The existing `run.py` only implements the probe subset. Its manifest is a
research receipt, with observations rather than an execution witness and no
scenario-control or fault-history support. The current immutable input contract
also needs stronger product-level verification and a content store in #2.

For #10 integration, consume its reviewed pinned source/artifact set, adapt
bootstrap and completion from Seedbox-specific ports to a QEMU-compatible guest
path, and keep its namespace packet topology, service binaries and checker.
Seed before starting any service or fault controller; ensure scenario sleeps
use guest virtual time and every entropy source is controlled. Do not inject
scenario commands according to host wall time or serial arrival timing.
Requalify delayed delivery, partition/heal, process stop/restart, actual recovery,
seed sensitivity and negative controls under TCG. Establish a meaningful execution
witness beyond these console observations before claiming seeded replay support.

Guest-local networking need not expose every packet through a host `run_until`
API. Host-fabric and guest-local fault implementations can share semantics and
acceptance requirements while using different mechanisms. Overlapping handles,
in-flight policy, action acknowledgments and failed recovery remain explicit
#3 requirements; #10's bounded fixture does not automatically implement them all.
Independent-machine reboot, disk durability, full snapshots and multi-QEMU causal
coordination remain unsupported, so a shared-kernel profile must reject requests
that require them.
