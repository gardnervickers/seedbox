# Staged AMD public-CLI smoke

This is source staging for [issue68](https://github.com/gardnervickers/seedbox/issues/68).
It has not qualified an engine or enabled a runner. Run only under a separately
reviewed finite hardware release on the compatible qualified AMD host as root.
The admission CLI must be bound to the exact frozen engine/kernel candidate;
there is no engine-change override. The current admission policy still targets
engine168ad/kernel e8ece. Final combined admission/CPUID-selected profile integration
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
`--performance-replay` for changed-performance MATCH. That replay changes both settings
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
store/run outputs, including unnormalized VMM reports. Preserve these on failure.
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

`harness/host-check.sh` plans from committed changes against the Git merge base,
using the checked-in [path map](../host-check-map.json). It defaults to `HEAD`
against the local `origin/main` ref; it never fetches. Review the plan first:

```sh
bash harness/host-check.sh --plan --base origin/main SHA
bash harness/host-check.sh --plan --pairs 4 SHA1 SHA2 SHA3
bash harness/host-check.sh --plan --train train-branch
```

A SHA list unions each SHA's merge-base diff and executes that union **once on
the last listed SHA**. List the integrated tip last when checking a train.
`--train BRANCH` uses the branch tip's cumulative merge-base diff, including all
commits since its divergence from the base. `--base REF` selects another local
base. Renames select both old and new paths; deleted paths also select checks.
Uncommitted changes are excluded. Reasons retain each matching path and revision.
`--plan` works on macOS without runtime inputs, topology probes, worktrees, builds,
leases, sudo or guests. A plan with no checks exits without host work even when
`--plan` is omitted.

Docs, CLI boundary/text, harness and explicit portable paths select no host
checks. Every change under `crates/seedbox-vmm/`, `kernel/` or `guest/` selects
one fixed full-state run, its default retained replay and `cargo test-vmm`.
Only stepping/counter/undo paths add `--undo 0 --margin 16384`; there is no flag
matrix. Disk paths add the four `disk-*.json` fixtures, network paths add `net`,
`net-pause`, `net-crash` and `net-crash-fork`, Compose supervisor/init paths add
Redis, etcd and the race YAML workloads, and timebase paths add ABI1/ABI2 PV
variants of `smoke.json`. The map names concrete source paths, globs and checks.
Shared `machine.rs`, `main.rs` and `devices.rs` additionally match category
symbols in changed lines and hunk function labels, excluding unchanged context.
This keeps a disk-only hunk out of the undo/margin checks. Path/hunk selection
is a reviewable coverage heuristic, not semantic proof; review its printed
reasons when changing shared code. An unmapped engine path gets the core plus
a warning; unknown paths elsewhere also get the core and a review warning.
Update the map when adding a subsystem or moving its code.

Execute only on the compatible Linux AMD host, inside the project's development
shell as the checkout owner. `--pairs N` (default 1, range 1..15) requires N free,
online, allowed, reciprocal SMT pairs. CPU 0 and sibling 16 are always excluded,
as is the whole discovered CPU 0 pair. Capacity is checked before builds and
worktree creation; busy leases, other coordinators and pinned userspace workloads
are skipped. Missing or unsafe canonical leases fail closed. Allocation does
not wait or steal a busy pair.

```sh
# Rebuilt candidate: retains an unqualified development-engine marker.
export SEEDBOX_KERNEL=/absolute/accepted-bzImage
export PINNED_ENGINE=/absolute/stored-pinned-seedbox-vmm
bash harness/host-check.sh --pairs 4 --train train-branch

# For timebase changes, also supply already built, distinct PV kernel artifacts.
bash harness/host-check.sh --pairs 4 --train timebase-train \
  --pv-kernel abi1=/absolute/abi1-bzImage \
  --pv-kernel abi2=/absolute/abi2-bzImage
```

`SEEDBOX_BUSYBOX`, `SEEDBOX_STATIC_CC` and (for disks) `SEEDBOX_MKE2FS` come from
the project shell. No kernel is built and no pins or admission policy are changed.
There is no current PV runtime switch: the ABI labels identify caller-supplied
kernel variants. Each variant must already be reviewed for the intended ABI;
its repeated runs are compared within that variant, never across kernels.
These raw fixture results do not qualify either kernel through public admission.
For a stored candidate, set `SEEDBOX_VMM` instead of rebuilding the engine.
That mode checks the stored bytes, not this revision's VMM sources. A rebuilt
engine embeds its build path and is never pinned. `PINNED_ENGINE` supplies the
pinned engine for the unchanged wrong-kernel refusal control and enables the
explicit unqualified development mode for successful public run/replay phases.
The runner adds no engine-change or unqualified-CPU override.

One detached target worktree is built and source-checked before dispatch. Builds
and portable checks are niced and unpinned. Public `doctor` admits the candidate
and every assigned lane before any raw fixture or guest is dispatched; a refusal
stops the whole invocation. The core run and dependent replays
stay together on one lane; independent KVM-unit, disk, link, Compose and PV
checks run concurrently on the other lanes, with one worker per pair. Each
Compose workload gets a default retained replay, with no performance variations.
Raw fixtures run twice sequentially on their assigned pair at seed 1009 and
compare world/full-state witnesses after checking success. Fork fixtures compare
cold seeds 1010/1011 against restoration in one `1009,1010,1011` process. Each
raw invocation has a 120-second limit. Public smoke/workload campaigns retain
the five-minute allocation and cleanup reserve. The unit command uses the
existing owned-session wrapper with a thirty-minute allocation and cleanup reserve.
No retries or extra seed sweeps
are performed. `cargo test-vmm` uses its normal default test set; ignored,
separately gated hardware diagnostics remain ignored.

Workers use the existing canonical `/tmp/seedbox-msr-cpu-N.lock` leases through
the public CLI or `batch.sh` supervisor; the unit-test worker holds those leases
while running `cargo test-vmm` pinned to its assigned CPU. Allocation briefly
probes canonical leases, then releases them for the child. Additional
`/tmp/seedbox-host-check-cpu-N.lock` coordinator reservations stay held between
commands and prevent another host-check invocation assigning the same pair.
Canonical locks are never unlinked or created by the allocator. Other direct
CLI/batch clients still arbitrate through canonical locks, so contention after
allocation fails the check. Catchable cancellation or a failed worker stops new
dispatch, signals only owned workers and joins them before releasing reservations;
cleanup that does not return keeps the coordinator waiting. Restoration and
ownership uncertainty are failures.

`HOST_CHECK_DIR` selects the evidence parent (default the system temporary
directory). Both pass and failure retain the worktree, plan, job specs, command
logs, state and `summary.json`. The summary keeps source/check wall times,
assigned pairs, undispatched jobs and smoke phase timings, including failures.
The fixed-run time includes preparation; default and performance replay times
are separate. Remove retained worktrees/evidence only after reviewing them.

The portable map, Git planner, allocator and scheduler controls live under
`harness/tests/test_host*.py` and run through `harness/acceptance.sh portable`.
They never access real MSRs/KVM or establish Linux runtime acceptance. Actual
parallel host dispatch, PV variant behavior, signal cleanup and finite-workload
budgets still require separately authorized hardware evidence.
