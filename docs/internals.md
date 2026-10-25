# Technical details

## Bounded clock reads and progress

PV clock reads charge time but yield before crossing the VMM's published bound.
A computation-only loop instead advances through DynamoRIO block-entry budgets and charged syscalls.
x86 kernel cpu_relax hooks also charge bounded reads with interrupts masked; arbitrary assembly loops do not.
The [client](../crates/seedbox-machine/guest/progress/progress.c) also replaces x86 RDRAND/RDSEED with guest entropy.

Syscall charges sample their price once. Each segment keeps its first clock read
and interrupt unmask, then charges the remaining ticks together up to the published
bound. A stopped continuation retains the unpaid charge in guest state.

Some syscalls are not application work at full price. The PV clock leaves the vDSO
without a clock on both architectures, so time syscalls (`clock_gettime`,
`clock_getres`, `gettimeofday`, and x86 `time`) cost only the clock read they make.
The translator also makes syscalls for itself: suspending threads to flush rewritten
code, returning from its signal handler and waiting on futexes. On ARM its syscall
stubs put a fixed mark in x7, which arm64 syscalls leave unused, and the kernel
charges a marked syscall a tenth of a syscall. A JVM makes millions of these while
it starts, and at full price they dominated simulated startup time. x86 has no such
mark yet and charges translator syscalls in full, as system time.

Guest time advances only inside the kernel, so every timer tick interrupts kernel
mode and tick accounting would charge all translated compute as system time
(`utime` zero, `ITIMER_VIRTUAL` never firing). The progress client passes a fixed
first argument to its `getpid`, and on both architectures a tick that interrupts
that call's charge directly counts as user time. RCU and the scheduler still see
kernel mode, and the charge itself is unchanged.

Progress instrumentation saves a fixed scratch register in thread-local storage,
decrements the block budget without changing application flags, and restores the
register before application execution. Exhaustion branches to one shared routine
instead of emitting a context-save sequence in every block. Its clean call charges
progress and redirects through DynamoRIO's dispatcher for pending signals and JIT
synchronization; the resumed block's repeated charge is refunded. Intermediate
scratch instructions have no application translation and cannot be relocation
points. Reconstructed instruction lists also let the client restore the scratch
value in reported contexts; stored translation tables alone do not.

## Reference TCG machine

TCG budgets QEMU icount steps, not retired instructions: REP I/O costs per iteration plus a final check.
Idle reports its next virtual timer but cannot jump time itself; the Driver advances the shared clock.
A bounded run has no host read timeout; only test-support adds a 600-second stuck-process watchdog.
The [adapter](../crates/seedbox-machine-tcg/src/machine/tcg.rs) merges frame, console and lifecycle output in occurrence order.

## Machine time bases and clock faults

Drift scales the whole machine, not just clock reads, using an exact piecewise mapping in 1e-9 ns units.
Inputs round down into machine time; outputs/deadlines map to the earliest simulation nanosecond reaching them.
Rate changes preserve accrued drift and remap pending timers; control payload times remain simulation time.
The [projection](../crates/seedbox-driver/src/clock/projection.rs) belongs to the Driver, outside Machine snapshots.

## Wall time across reboots

Fresh execution derives its epoch from image creation times, not the host clock, with an 86400-second floor.
Every boot adds the machine counter to that epoch; kernel monotonic time alone would restart at reboot.
Realtime steps change kernel state and expire with the boot, whereas Driver-owned drift survives it.
The [supervisor](../crates/seedbox-machine/src/pv/guest_service_init.c) initializes realtime before announcing Ready.

## Prepared boot inputs

Repeated OCI preparation reuses finished boot archives and logical filesystem views
through content-store records. The [key](../crates/seedbox-driver-execution/src/driver_world/preparation/cache.rs)
covers runtime/engine bytes, the logical rootfs, resolved launch and mounted configs,
generated machine setup, epoch, progress budget and memory. Hits verify both retained
objects; missing objects render again. Image resolution, runtime validation, boot
memory checks and retention of the complete replay closure still run on every preparation.

## Same-time controls

Committed outputs precede due timeline items, then frames, then timer wakeups; same-time items are FIFO.
A trigger found during commit can admit policy before a prequeued host input due at that same boundary.
Plans created by admission append behind existing items; delayed guest replies belong only to the calling boot.
The [Driver](../crates/seedbox-driver/src/driver.rs) assigns fault IDs at admission, never for future overrides.

## Machine network delivery

Partitions survive reboots and stack by handle, but do not retract frames already routed.
A frame stamped T by the round ending at T routes before a partition/heal due at T.
Admission-run output after that effect sees the changed policy; zero-budget receive can wake a paused machine.
The [network policy](../crates/seedbox-driver/src/driver/network.rs) checks cuts before random draws and FIFO reservation.

## Disk state at boot end

Device reset preserves durable and volatile overlays; boot end discards the volatile layer exactly once.
Shutdown merely requests guest cleanup: only guest FLUSH promotes writes, and background writeback may precede it.
Live snapshots retain both layers without promotion; terminal snapshots contain boot-end policy already applied.
The shared [store](../crates/seedbox-machine/src/machine/disk.rs) completes requests synchronously and retains completed prefixes on error.

## Guest out-of-memory reports

The supervisor retains `/dev/kmsg` before entering the application image and
relays the supported Linux kernel's OOM victim records on the management channel.
Reports carry the victim PID, task name and anonymous RSS, before the application
outcome. Only a report naming the supervised leader explains `137 (out of memory)`;
an ordinary SIGKILL remains `137`. Boot identity comes from the Driver; each outcome consumes its own pending
report, so restarted applications cannot reuse an earlier kill. The world retains
the newest 64 reports, including their boots; diagnostics suggests raising `memoryMiB`.
Lost kernel records fail supervision with an I/O error rather than guessing a victim.

## ARM exclusive reservations

Host interruption can clear a hardware reservation without any guest write, making raw store-exclusive failure nondeterministic.
Translation retains address/value/size/guest epoch and retries physical monitor loss inside the exclusive pair.
Guest boundaries advance the epoch; watchdog and maintenance exits do not. Value equality alone misses ABA.
The [translator](../vendor/dynamorio/core/arch/aarchxx/mangle.c) and kernel return hook assume one guest CPU.

## Editing DynamoRIO

[vendor/dynamorio](../vendor/dynamorio) is the maintained
[Seedbox fork](https://github.com/gardnervickers/dynamorio/tree/seedbox/release_11.3.0-1-backports),
based on upstream `release_11.3.0-1` with the four existing Seedbox backports.
The gitlink selects its source; [the Nix recipe](../crates/seedbox-machine/dynamorio.nix)
applies packaging adjustments and builds the launcher, core, public extensions and
SDK together. The progress client and test clients use that exact build's generated
headers, CMake exports and libraries. Source version metadata can remain 11.3;
the selected gitlink, rather than a release archive label, identifies the API build.
The recipe preserves C-only compiler flags for the baseline's type probes and uses
RelWithDebInfo to match its public SDK exports.

Follow the [README setup](../README.md#build) after cloning or switching Seedbox
revisions. Shell entry excludes submodules: direnv and
[the local reference helper](../tools/dev-flake.sh) use a plain `git+file` reference.
The helper URL-encodes the current checkout root, including linked Git worktrees
and paths containing spaces or URL syntax characters.

`cargo xtask build` and `build-assets` initialize missing submodules before
preparing assets. For direct Nix asset or fixture builds, initialize explicitly
and request submodules on the initial fetch so Nix includes live DR edits:

```sh
bash tools/bootstrap-submodules.sh
seedbox_flake="$(bash tools/dev-flake.sh)?submodules=1"
nix build "$seedbox_flake#machinePvAssets"
```

Bare `.#machinePvAssets` excludes the translator source. The maintained fork has
no nested gitlinks: Seedbox's SDK, progress client and test fixtures do not use
upstream's optional elfutils, libipt or zlib source checkouts. drsyms remains
excluded from the SDK build.

[Asset bootstrap](../tools/bootstrap-submodules.sh) reads NUL-delimited index
gitlinks and initializes each missing checkout separately, then visits its
descendants. Initialized checkouts are never updated, even when their HEAD differs
from the gitlink. To deliberately align sources with a Seedbox revision, first
save local work, then run `git submodule update --init vendor/dynamorio`.

For coordinated edits, switch the submodule to `seedbox/release_11.3.0-1-backports`,
edit and verify it with Seedbox's PV asset build, then commit and push DR first.
Commit `vendor/dynamorio` in Seedbox to record the published DR commit and push
the Seedbox branch. Add new DR files to its index so local Nix builds include them.
The Mac builder exports tracked submodule files with local edits, excluding Git
metadata and ignored build output, to its Linux container.

## External DynamoRIO tests

The `dynamorio` named suite runs native Linux x86-64/ARM64 application fixtures
from [the maintained DR fork](../vendor/dynamorio/seedbox-tests),
with native controls, the packaged `drrun`, and a public drmgr/drreg client.
The fork owns the case sources, corpora, clients and case-specific runners.
Seedbox's existing Cargo targets compile the fork's Rust entrypoints directly,
preserving libtest names and feature gates. The manually dispatched native
integration workflow runs both architectures; ordinary Seedbox CI remains automatic.
Nix packages fixtures directly from the selected submodule, using its matching
SDK/runtime. General Seedbox test support and guest/PV integration stay here.
The time policy scans only `seedbox-tests/` within the fork, including its Rust
helpers and the narrowly marked native syscall subjects.
It checks signal context/vector/mask/alternate-stack restoration, live registers
and flags under instrumentation, and ARM load/store-exclusive application semantics.
Source-derived clients also check public flush operations, finite concurrent
memory queries, managed-runtime result models and JVM JIT floors. ARM-only codec,
hardware-instruction and exclusive populations remain explicit architecture cases.
Fixture `origins.json` records source paths, commits, hashes and adaptations;
retained saved inputs alone are not replay coverage. Client execution is asserted
independently. These host tests do not validate the
Seedbox guest epoch or executable-FD handoff, nor require KVM, root or Docker.

From the checkout root inside the development shell:

```sh
bash tools/bootstrap-submodules.sh
seedbox_flake="$(bash tools/dev-flake.sh)?submodules=1"
export SEEDBOX_DYNAMORIO_FIXTURES=$(nix build "$seedbox_flake#dynamorioTestFixtures" --no-link --print-out-paths)
export SEEDBOX_DYNAMORIO_DEBUG_FIXTURES=$(nix build "$seedbox_flake#dynamorioDebugTestFixtures" --no-link --print-out-paths)
bash harness/check.sh dynamorio
```

The checker requires prepared fixtures before building and runs portable checks
first; portable's `--all-features --no-run` compiles the integration entrypoint.
Cargo discovers cases in the feature-gated target. For a focused development run,
use `cargo test -p seedbox-machine --features dynamorio-tests --test dynamorio`.
The separate debug fixture package builds its public SDK, runtime and clients
from the same source with `Debug`; `prof_pcs` and replay controls require that
explicit input. Shared IR and A64 selector controls run in public decoder and
standalone-core modes against both profiles. The original 50 saved inputs and
full eight transform populations require public decoder mode; their full
standalone populations remain manual supplemental diagnostics.
Signal storm and interval-timer models retain their source populations and pure
clock-input oracles. Live elapsed-clock timing equivalence remains incomplete.
[The hosted workflow](../.github/workflows/dynamorio-tests.yml) is manual-only
because the complete native suite is expensive. After updating `vendor/dynamorio`,
open **DynamoRIO integration** in GitHub Actions, choose **Run workflow**, select
the Seedbox ref containing the update, and require both the x86-64 and ARM64 jobs.
GitHub accepts a manual dispatch only when the workflow file is present on the
default branch, so a newly added workflow cannot run before that file is merged.

Ordinary Linux checker jobs share one sequential subreaper owner. Cancellation
first signals TERM so fixture supervisors can finish cleanup; after two seconds
it kills direct children and newly adopted descendants across process groups.
It requires the kernel's complete reaping receipt within ten seconds. Managed
runtime phases use the native supervisor too, retain bounded stdout/stderr, and
stream bounded phase diagnostics. DR libtest failures print immediately so a
later target timeout does not discard earlier assertion failures.

The separate `dynamorio-isolated` suite requires positive namespace and cgroup
preflights before any build. `seedbox-dr-isolate pid1 -- COMMAND...` creates a fresh
user/PID namespace and execs the application directly as PID1. On hosts restricting
unprivileged namespaces, `SEEDBOX_DYNAMORIO_PID1_LAUNCHER` may select the packaged
`seedbox-dr-pid1` wrapper: it first probes unprivileged support, then uses a scoped
sudo namespace controller whose child drops groups/UID/GID before exec. The suite
and builds run as the ordinary user.

For failed-clone process limits, `SEEDBOX_DYNAMORIO_CGROUP_ROOT` supplies an empty,
owned cgroup-v2 domain with `pids` delegated. The launcher creates a unique leaf,
gates its child until membership is installed, and records the actual
`pids.events max` delta; a missing limit hit fails. Its controller stays outside
the limit, terminates surviving owned descendants and removes the leaf.
[The CI supervisor](../tools/dynamorio-ci/cgroup.py) creates a unique domain and
unlimited controller subgroup, enables `pids` only inside that domain, and runs its
normal-user child there. It cleans that domain on completion or interruption. A
parent that has not already delegated `pids` is a setup failure; the helper never
changes shared controllers, global sysctls or unrelated process membership.
Ordinary `dynamorio` remains accessible without these isolated-fixture prerequisites.

## ARM idle rendezvous

Making an interrupt pending while idle has interrupts masked can slide delivery past a later unmask.
That changes saved kernel state and entropy even when application output looks identical.
The patched idle path unmasks before the stopped PV rendezvous, then restores the caller's mask on return.
The [kernel hook](../crates/seedbox-machine/guest/kernel/arm64/seedbox.c) lets the VMM assert the line at that exact boundary.

## Canonical saved vector state

XSAVE buffers can restore identical initial SSE or zero YMM state while differing in presence bits or ignored payload bytes.
Host interruptions can expose those differences in full-RAM comparison despite equivalent architectural state.
Kernel task-state saves canonicalize zero/absent YMM; compacted saves also canonicalize initial/absent SSE, preserving noninitial XMM and MXCSR values.
Standard-format SSE remains unchanged because its MXCSR restore rules differ; direct application XSAVE and some signal paths remain outside the task-save hook.
The [patch](../crates/seedbox-machine/guest/kernel/x86_64/apply.py) changes guest bytes, not the comparison's coverage.
Live vCPU vector evidence hashes each advertised component's effective value, substituting init state when absent.
The [digest](../crates/seedbox-machine-pv/src/machine/pv/worker/x86_64/backend/evidence.rs) omits XSTATE_BV, so host FPU save/restore cannot change it.

## Checkpoint capture

A held, admitted fault is capturable; a prepared but uncommitted intervention returns FaultPending.
Capture needs paused, application-ready execution at a committed boundary with no outstanding I/O cost.
It consumes the required commit handoff before optional publication, without waiting for observers or releasing held effects.
The [service envelope](../crates/seedbox-driver-execution/src/driver_world/checkpoint.rs) retains input authority, not observation history.

## Connection incarnations

Connection selection follows observed IPv4 TCP sends, including packets later dropped, not the guest's TCP state.
Tuple reuse creates a new incarnation; a fault bound to the old one cannot silently attach to its replacement.
IPv6 and fragmented IPv4 bypass connection selection but still follow ordinary link policy.
The [classifier](../crates/seedbox-engine-core/src/connections.rs) counts actual drops; arming alone proves none occurred.

## Archive publication and cleanup

A complete import upload may publish after disconnect; retries are not idempotent and cannot be automatic.
Downloads publish by atomic no-clobber link; staging cleanup unlinks only its exclusively created payload.
Directory removal also requires proven ownership, safe parent permissions and absent ACLs; unprovable parents retain it.
The [transfer code](../crates/seedbox-client/src/transfers.rs) does not treat stat/remove as atomic against same-UID mutation.

## Build asset publication

`cargo xtask build` prepares assets, then forwards its options to `cargo build`.
Linux uses Nix directly; Mac uses Docker or Podman for guest inputs and native Nix
for host tools. `cargo xtask build-assets` prepares assets without compiling the CLI.
Unchanged bundles reuse `build/assets`; changed bundles retain the previous generation
for concurrent builds. With all Cargo builds stopped, `cargo xtask clean-assets`
removes old generations while keeping the current bundle. Built executables embed
their own assets. See [xtask](../crates/xtask/src/main.rs).
