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
operations, requires fixed full-state PASS, then retained default and
changed-performance MATCH. One replay changes both settings
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

## One PR's host check

`harness/host-check.sh REV CPU` runs the whole per-PR host check for one commit
on one SMT pair: it makes a worktree, builds the CLI and engine, runs fmt,
clippy and `harness/acceptance.sh portable`, then this smoke tier, and prints
each step's time. Set `SEEDBOX_KERNEL` to the accepted kernel. A rebuilt engine
embeds its build path, so it is never pinned: for the pinned tier set
`SEEDBOX_VMM` to the stored pinned engine (the build of it is skipped), and to
check a rebuilt engine set `PINNED_ENGINE` for the development tier. Checks
of different commits can run at once on different pairs; builds run niced and
unpinned, and only the smoke tier uses the pair. A pass deletes the worktree
and evidence and keeps `~/host-check-SHA-cpuN-XXXX.txt` with step and phase times; a failure keeps everything
and prints its path.

After `bash harness/mac-check.sh REV` passes on macOS in the project Nix shell,
use `harness/host-check.sh --skip-portable REV CPU` to skip fmt, clippy and the
portable suite's workspace/Python checks and YAML validation. VMM tests still
run explicitly, and the CLI build, conditional VMM build and smoke tier remain.
Both scripts print the full checked SHA so the Mac and host results can be
matched. The Mac gate retains failed worktrees and deletes passing ones.
