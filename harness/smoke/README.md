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

The wrapper never builds the CLI, engine, kernel or supervisor. The public `run`
command prepares the existing fixed `examples/faults` application, so OCI
preparation is part of that command's measured wall time. Build/pull costs are
separate prerequisites; do not describe a warm execution receipt as a fresh install.
`prepare` JSON cannot be fed into `run`; this tier adds no prepared-input adapter.

The finite normal tier refuses a private wrong-content kernel before its good-input
operations, requires fixed full-state PASS, then retained default, changed-margin
and changed-undo MATCH. Replays use the retained engine/kernel without overrides.
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
