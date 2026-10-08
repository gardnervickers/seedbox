# Staged AMD public-CLI smoke

This is source staging for [issue68](https://github.com/gardnervickers/seedbox/issues/68).
It has not qualified an engine or enabled a runner. Run only under a separately
reviewed finite hardware release on the compatible qualified AMD host as root.
Engines and guest kernels are not pinned; the smoke records the hashes it ran.
Host-kernel bytes are not checked.
Final combined admission/CPUID-selected profile integration
is a compatibility prerequisite; this entrypoint never passes
`--allow-unqualified-cpu` or `--allow-engine-change`. A merged engine PR does not update or qualify that policy.

From a reviewed checkout in its project Nix environment, supply already built,
frozen input files and the explicitly admitted preparation-tool PATH:

```sh
python3 harness/smoke/amd.py --cli /absolute/seedbox \
  --engine /absolute/seedbox-vmm --kernel /absolute/bzImage \
  --guest-init /absolute/compose-init --busybox /absolute/busybox \
  --cpu CPU --tool-path "$PATH" --output /absolute/FRESH-EVIDENCE
```

The wrapper does not build Seedbox CLI, VMM, kernel, supervisor or toolchain
artifacts; those frozen artifacts are prerequisites. The baseline public `run`
generates the fixed `examples/faults` project into the evidence directory with
`examples/faults/generate.py`, prepares it, and may build its workload images
or pull missing base images. All of that preparation is charged within the
baseline command's wall-time budget and the five-minute campaign allocation.
The baseline timer includes preparation and execution; there is no separate
preparation-only or execution-only timer. Replays are timed separately. These
measurements do not establish cold-install performance.
`prepare` JSON cannot be fed into `run`; this tier adds no prepared-input adapter.

The finite normal tier refuses a private wrong-content kernel before its good-input
operations, requires fixed full-state PASS, then retained default MATCH.
For stepping, counter or undo changes, add
`--perf-replay` for changed-performance MATCH. That replay changes both settings
(`--undo 0 --margin 16384`): with undo on, the counter's final approach uses the
undo margin and never reads `--margin`. Undo-off stepping costs about 17x a
default replay on this fixture, so the margin is the profile's smallest adaptive
one. Replays use the retained engine/kernel without overrides. `result.json`,
`failure.json` and stdout list each phase's wall time.
It verifies the complete declared object closure, every retained log object's
hash, semantic identity, full-state/witness/fault comparison and all opaque guest
console/management bytes. Omitted logs or absent full-state evidence fail this tier.
Application content stays unparsed. Host timing and counters are retained as raw
measurements, not compared as deterministic results.

One five-minute overall allocation includes setup, preparation, execution and
readback, with a thirty-second cleanup reserve. The wrapper uses the reviewed
batch46 unreaped-session ownership code, polls and retains runtime-owned MSR
originals, then checks both originals/restoration, snapshot removal and canonical
lease device/inode/free status before outcome assertions. Canonical leases must
already exist from host qualification; the wrapper never creates or unlinks them.
It neither modifies affinity nor writes MSRs itself. The released pair must be
reserved by the coordinator, online, allowed and reciprocal, with no conflicting
userspace pinned workload. It does not hold the same leases across the child CLI,
which owns them during execution. Uncertain process or cleanup evidence stops
later operations and fails the campaign; there are no retries.

The output directory must be new. It retains commands, stdout/stderr, lifecycle
samples, owned MSR originals, cleanup, public inspect documents and the full state
store/run outputs, including unnormalized VMM reports, on failure. On a pass,
it deletes the state store and keeps phase receipts. On failure it prints the
command to delete its owned state after review.
An owner result remains pending independent evidence review. Timings for default
and changed-performance phases are separate; fixed-run wall includes preparation.
The measured earlier five-execution demo took122.600s, but neither that receipt
nor portable controls establish this full-state tier's runtime or five-minute fit.

Broken-fixture acceptance, real cancellation, competing-job reservation and a
deliberately bad **engine** candidate remain OPEN runner acceptance gates. The
normal tier does not close68, prove cross-host/version behavior, qualify boot
fork/vDSO/new kernel features, or replace StageB. No workflow or privileged host
service is installed. Portable controls run through `harness/acceptance.sh portable`;
they test the harness's refusal logic, not guest correctness.

The smoke requires that new runs carry no `engine_qualification` marker in
identity, run report or doctor JSON. Doctor receipts are retained.

## Change-scoped host checks

`harness/host-plan.py BASE HEAD` reads `git diff --name-only BASE...HEAD` and
prints every selected check with the path that selected it. `--json` emits the
same plan as JSON. Repeatable `--extra CHECK` adds reviewer checks; selection
unions all matching rows and additions. Engine comparison selects `cluster-fixed`
when engine inputs change; fixture-input rows and explicit reviewer additions
always keep it.
The [checked-in map](../host-check-map.json) defines these rows:

| Changed paths | Host checks |
|---|---|
| only *.md, docs/, crates/*/tests/, harness/tests/, python/tests/, templates/, crates/*/templates/, tools/mac-tcg/, or CLI help text | none (portable checks only) |
| crates/seedbox-vmm/**, guest/**, or the CLI execution path (vmm.rs, host.rs, admission.rs, counter.rs, orchestration.rs) | core: doctor, one fixed --full-state run, one default replay (MATCH) |
| stepper.rs, machine/counted.rs, timebase.rs, boundary.rs, counter.rs, undo/checkpoint code | core + one --undo 0 --margin 16384 replay |
| disk.rs, virtio-blk, guest/disk*, build-disk.sh | core + disk-crash and disk-ext4-crash worlds |
| network, link, faults routing | core + net, net-pause, net-crash worlds |
| crates/seedbox-engine-core/src/links.rs, crates/seedbox-engine-core/src/connections.rs | existing automatic checks + reviewer L1/L2 shaped replay and P1 == M1 plain feature-off comparison |
| shared machine/main/devices changed lines mentioning undo/checkpoint/rollback/margin/stepping/counter | existing automatic checks + reviewer short U1/U2 undo invariance |
| control.rs, host control, python/seedbox/control.py | core + control-cases (run, then replay) |
| guest/compose-init.c, preparation, images, volumes | core + sanity (+ volume-crash if volumes changed) |
| examples/faults/**, harness/smoke/**, harness/host*, python/seedbox/** | core (control.py also keeps control-cases) |
| harness/worlds/<name>.json | the named fixture's check when mapped, otherwise core; other matching rows can add checks |
| harness/batch.*, harness/worlds/cluster-fixed.json, examples/etcd/workload/** | core + cluster-fixed, even with identical engine inputs |
| examples/<other>/** | core + "example <name>: its reviewer's run list is required" |
| flake.nix, flake.lock, rust-toolchain*, Cargo.lock, .cargo/**, harness/vmm-rustc.py, crates/seedbox-control/** | core (control source also keeps control-cases) |
| kernel/ | core + "kernel campaign needed"; execution refuses incomplete coverage |
| anything else unmatched | core (fail closed) |

The plan names `core` (public CLI smoke) and `cluster-fixed` separately so their
execution and comparison times are visible. For engine comparison, `cluster-fixed`
runs when VMM, guest or toolchain/build inputs differ from local `origin/main`. The
planner compares exact trees even when `--base` uses another ref, so engine-only
changes with merged or reverted inputs equal to main skip the second build and
both cluster runs. The batch runner, fixed world and cluster workload rows set
`"always": true` in the map: their checks are explicit, so `cluster-fixed` exercises those inputs
even when the engine trees match. Cluster workload changes also retain the etcd
example's reviewer-run-list requirement. Reviewer `--extra cluster-fixed` still
forces the comparison. `perf-replay`
is the single opt-in undo-off/margin replay. A reviewer adding `core` or `perf-replay` also selects
its core dependencies. Other additions select exactly the named check.
The performance replay uses the generated short fixed `examples/faults/compose.yaml`
workload within the smoke's 300-second campaign allocation.
Shared machine code adds disk, network or undo checks when changed lines touch
those controls. Shared preparation/supervisor code adds `volume-crash` when
changed lines touch volumes. Changed comment/help lines in the CLI need only
portable checks; executable changes fall back to core. Unchanged diff context
and file headers do not select extra checks. Review the printed reasons for
shared code; this mapping does not prove semantic coverage. A missed keyword
only loses an extra fixture, never core.

The `links` category also emits named reviewer runs in the plan's `runs` JSON
field and text output, with world, engine, seed, timeout, comparison and path
reasons. L1/L2 use `harness/worlds/shaped.json` on the target engine at seed 1
and require matching witnesses. P1 uses `harness/worlds/plain.json` on the
target; M1 uses the same plain world and seed on main, requiring feature-off
witness equality. Each has an 1800-second timeout. These fixtures are being
promoted by PR #97; the planner references them by name without copying them.

Only the existing `undo-checkpoint-shared` diff-gated rule adds U1/U2, both on
PR #97's `harness/worlds/shaped-tol-short.json` at seed 1 with a 600-second
timeout. U1 uses default undo; U2 disables undo, with the default adaptive
margin. Their witnesses must match. Link changes alone, dedicated stepping/undo
files and reviewer `--extra perf-replay` do not select this pair. The full
`shaped-tol.json` U pair is never selected. Every planned undo-off or margin
entry uses a short workload with a timeout no greater than 600 seconds.

These named runs are reviewer requirements, not automatic worker jobs. They
remain visible as `not run` in host-check summaries and the final status;
automatic PASS does not close them. Execute only the named entries through
the existing batch supervisor after PR #97's fixtures are available. Do not
invoke the whole `harness/run-links.sh` campaign for a scoped plan, since that
would add U1/U2 to link-only changes.

```sh
python3 harness/host-plan.py origin/main HEAD --json --extra net
bash harness/host-check.sh --plan --base origin/main REV
bash harness/host-check.sh --plan REV --extra control-cases --extra disk-crash
```

`host-check.sh --plan` prints the plan for `--base` (default `origin/main`)
through `REV` (default `HEAD`) and exits without host probes, builds or state.
Refs are local; the planner never fetches. Rename detection is disabled so both
old and new paths retain their coverage; deleted paths also select checks. An empty selection prints `host: none`
and exits 0, including when `--plan` is omitted. Kernel changes print
`kernel campaign needed` and select core, never `host: none`; execution refuses
to claim coverage of such a plan.

Execute in the project development shell on the qualified Linux AMD host:

```sh
export SEEDBOX_KERNEL=/absolute/accepted-bzImage
export SEEDBOX_DISK_KERNEL=/absolute/virtio-blk-bzImage # only when disk/volume rows are planned
export SEEDBOX_ETCD=/absolute/static-etcd
bash harness/host-check.sh --lanes 1,2,3 REV --extra control-cases
```

`--lanes 1,2,3` selects exactly SMT pairs `(1,17)`, `(2,18)`, `(3,19)` in that
order, with one worker per pair. `host-check.sh REV CPU` remains shorthand for
`host-check.sh REV --lanes CPU`. Do not combine CPU and `--lanes`. Numbers must be distinct and in 1..15; CPU 0
and 16 are refused. Each pair must be online, allowed, reciprocal and free.
The allocator holds separate coordinator reservations and probes existing
canonical CPU/MSR leases, releasing the latter for the public CLI or batch
supervisor to own during execution. Busy or unsafe requested pairs fail before
builds; no replacement pair is silently selected. Cancellation/failure stops
new dispatch and joins owned workers before releasing reservations.

Host execution runs fmt, clippy and `harness/acceptance.sh portable` by default
in the target worktree before any guest dispatch. `--skip-portable` skips those
steps and runs `cargo test-vmm` explicitly, even with a stored engine. Use the
skip only after a Linux pre-check passed the same full SHA; a Mac pre-check omits
VMM and batch-supervisor tests. The plan prints the full target and base SHA.
Failed checks retain evidence and exit 1; setup/admission errors exit 2.

`SEEDBOX_BUSYBOX`, `SEEDBOX_STATIC_CC` and `SEEDBOX_MKE2FS` come from the project
shell. `SEEDBOX_ETCD` supplies a frozen static etcd binary for `cluster-fixed`;
its client and initramfs are built once and used with both engines.
`SEEDBOX_KERNEL` remains the accepted kernel for core, cluster and network checks.
Until K2 is accepted, `disk-crash`, `disk-ext4-crash` and the Compose
`volume-crash` check require a separate `SEEDBOX_DISK_KERNEL` with `VIRTIO_BLK`
and `EXT4_FS`. Missing/invalid disk input refuses before reservations, builds or
state creation, naming that variable. Every selected kernel goes through the
existing doctor admission on each lane. The main engine is built from a detached worktree of local `origin/main`, even when
`--base` selects another diff base. The summary records main's revision and
MATCH/DIFFERENT evidence; a difference is reported without treating a changed
engine as a replay mismatch. Failed fixture execution still fails the check.
No kernel is built.

`SEEDBOX_VMM` may select an already built candidate engine. Otherwise the target
VMM is built with `cargo build-vmm`.
Builds, including guest builders, run niced and without CPU pinning. Admission
checks each assigned lane before raw guest dispatch. Host execution runs the
selected public smoke, adds `--perf-replay` only if planned, and runs each
selected raw world once at seed 1009 with `--digest` through `harness/batch.sh`.
`cluster-fixed` runs once per engine on its assigned pair with identical inputs.
There are no extra fork, PV, KVM-unit or performance matrices. `margin_min`
remains 16384; the harness does not modify VMM source.

`control-cases` generates the existing focused lifecycle-error scenario, checks
its intentional workload FAIL, then requires default replay MATCH and verifies
both retained control transcripts. `sanity` runs the existing planted-missing-ack
YAML through public run/replay. `volume-crash` runs
`harness/acceptance/workloads/volume-crash.yaml` through public run/replay, requiring PASS then MATCH. Its etcd service mounts a named
volume and the driver crashes/reboots it to check acknowledged data survives.
The job passes the shell's `SEEDBOX_MKE2FS` explicitly as `--mke2fs` because the
public campaign uses a clean environment.
Raw commands have a 120-second limit; public campaigns use the smoke's finite
five-minute allocation and cleanup reserve. No check retries a failed run.

`HOST_CHECK_DIR` selects the evidence parent (default the system temporary
directory). `summary.json` and the text summary retain step and phase wall times,
assigned pairs, reasons, selected kernels, undispatched jobs and the cluster
comparison. Reviewer requirements appear in both summaries and repeat on the
final PASS/FAIL line as `not run`, so a passing mapped check cannot hide an
example's outstanding reviewer run list. Fixed-run wall time includes preparation. Passing public campaigns delete their state;
a passing host check removes its private worktrees and run state, keeping the
summaries beside the removed directory. Failures retain state and print an
explicit cleanup command. Cleanup failures are failures.

Portable controls run through `harness/acceptance.sh portable`. They cover every
mapping row, mixed changes, reviewer additions, Git planning, lane selection,
lease contention, dispatch, exact replay flags, comparison and cleanup. Linux
parallel dispatch, guest behavior and cancellation/restoration remain hardware
acceptance work; portable tests do not qualify an engine or host.

## Work per host check

Smoke now uses `batch=4`: 16 acknowledgements, healthy recovery of 4 then 12
records, and 12 lost values in the broken restart control. All faults, probes
and per-acknowledgement checks remain; the full example stays at `batch=32`.
[PR #110](https://github.com/gardnervickers/seedbox/pull/110) records the portable
evidence.
