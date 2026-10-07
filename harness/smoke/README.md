# Staged AMD public-CLI smoke

This is source staging for [issue68](https://github.com/gardnervickers/seedbox/issues/68).
It has not qualified an engine or enabled a runner. Run only under a separately
reviewed finite hardware release on the compatible qualified AMD host as root.
The admission CLI must be bound to the exact frozen engine/kernel candidate;
there is no engine-change override. The source admission policy selects the accepted engine and guest kernel. Final combined admission/CPUID-selected profile integration
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

For the optional development tier, add `--allow-dev-engine --pinned-engine
/absolute/PINNED-seedbox-vmm` to the invocation above. `--engine` and `--kernel`
select the frozen development artifacts; at least one must be unpinned. The
separate pinned engine is required so the wrong-kernel control still runs without
`--allow-dev-engine` and receives the existing guest-kernel refusal. Successful
run/replay phases receive the flag, and the wrapper requires `dev-unpinned` in
identity, the run report and doctor JSON, `dev engine: unqualified` in doctor
text, and labelled unqualified MATCH output. The default tier keeps pinned
admission and rejects development markers. Both tiers retain doctor receipts.
The development tier is unqualified evidence and does not change release policy.

## Change-scoped host checks

`harness/host-plan.py BASE HEAD` reads `git diff --name-only BASE...HEAD` and
prints every selected check with the path that selected it. `--json` emits the
same plan as JSON. Repeatable `--extra CHECK` adds reviewer checks; selection
unions all matching rows and additions, so none can drop an earlier selection.
The [checked-in map](../host-check-map.json) defines these rows:

| Changed paths | Host checks |
|---|---|
| only docs, *.md, CLI help text, portable Python/tests | none (Mac checks only) |
| crates/seedbox-vmm/**, guest/**, or the CLI execution path (vmm.rs, host.rs, admission.rs, counter.rs, orchestration.rs) | core: wrong-kernel refusal, doctor, one fixed --full-state run, one default replay (MATCH), cluster-fixed 1009 on this engine and on main's (report any difference) |
| stepper.rs, machine/counted.rs, timebase.rs, boundary.rs, counter.rs, undo/checkpoint code | core + one --undo 0 --margin 16384 replay |
| disk.rs, virtio-blk, guest/disk*, build-disk.sh | core + disk-crash and disk-ext4-crash worlds |
| network, link, faults routing | core + net, net-pause, net-crash worlds |
| control.rs, host control, python/seedbox/control.py | core + control-cases (run, then replay) |
| guest/compose-init.c, preparation, images, volumes | core + sanity (+ volume-crash if volumes changed) |
| kernel/ | not covered: print "kernel campaign needed" |
| anything unmatched under crates/ | core (fail safe) |

The plan names `core` (public CLI smoke) and `cluster-fixed` separately so their
execution and comparison times are visible. `perf-replay` is the single opt-in
undo-off/margin replay. A reviewer adding `core` or `perf-replay` also selects
its core dependencies. Other additions select exactly the named check.
Shared machine code adds disk, network or undo checks when changed lines touch
those controls. Shared preparation/supervisor code adds `volume-crash` when
changed lines touch volumes. Changed comment/help lines in the CLI need only
portable checks; executable changes fall back to core. Unchanged diff context
and file headers do not select extra checks. Review the printed reasons for
shared code; this mapping does not prove semantic coverage.

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
`kernel campaign needed`; execution refuses to claim coverage of such a plan.

Execute in the project development shell on the qualified Linux AMD host:

```sh
export SEEDBOX_KERNEL=/absolute/accepted-bzImage
export PINNED_ENGINE=/absolute/stored-pinned-seedbox-vmm
export SEEDBOX_ETCD=/absolute/static-etcd
bash harness/host-check.sh --lanes 1,2,3 REV --extra control-cases
```

`--lanes 1,2,3` selects exactly SMT pairs `(1,17)`, `(2,18)`, `(3,19)` in that
order, with one worker per pair. Numbers must be distinct and in 1..15; CPU 0
and 16 are refused. Each pair must be online, allowed, reciprocal and free.
The allocator holds separate coordinator reservations and probes existing
canonical CPU/MSR leases, releasing the latter for the public CLI or batch
supervisor to own during execution. Busy or unsafe requested pairs fail before
builds; no replacement pair is silently selected. Cancellation/failure stops
new dispatch and joins owned workers before releasing reservations.

`SEEDBOX_BUSYBOX`, `SEEDBOX_STATIC_CC` and `SEEDBOX_MKE2FS` come from the project
shell. `SEEDBOX_ETCD` supplies a frozen static etcd binary for `cluster-fixed`;
its client and initramfs are built once and used with both engines. The main
engine is built from a detached worktree of local `origin/main`, even when
`--base` selects another diff base. The summary records main's revision and
MATCH/DIFFERENT evidence; a difference is reported without treating a changed
engine as a replay mismatch. Failed fixture execution still fails the check.
No kernel is built, and no engine/kernel pins or admission policy are changed.

`SEEDBOX_VMM` may select an already built candidate engine. Otherwise the target
VMM is built with `cargo build-vmm`; `PINNED_ENGINE` supplies the unchanged
wrong-kernel control and enables the public CLI's explicit unqualified dev mode.
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
YAML through public run/replay. `volume-crash` is the low-level ext4 flushed-volume
crash/reboot ledger fixture, using the same retained-disk probe as
`disk-ext4-crash`; it does not establish persistent Compose volume support.
Raw commands have a 120-second limit; public campaigns use the smoke's finite
five-minute allocation and cleanup reserve. No check retries a failed run.

`HOST_CHECK_DIR` selects the evidence parent (default the system temporary
directory). `summary.json` and the text summary retain step and phase wall times,
assigned pairs, reasons, undispatched jobs and the cluster comparison. Fixed-run
wall time includes preparation. Passing public campaigns delete their state;
a passing host check removes its private worktrees and run state, keeping the
summaries beside the removed directory. Failures retain state and print an
explicit cleanup command. Cleanup failures are failures.

Portable controls run through `harness/acceptance.sh portable`. They cover every
mapping row, mixed changes, reviewer additions, Git planning, lane selection,
lease contention, dispatch, exact replay flags, comparison and cleanup. Linux
parallel dispatch, guest behavior and cancellation/restoration remain hardware
acceptance work; portable tests do not qualify an engine or host.
