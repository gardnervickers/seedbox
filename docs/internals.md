# Technical details

## Bounded clock reads and progress

PV clock reads charge time but yield before crossing the VMM's published bound.
A computation-only loop instead advances through DynamoRIO block-entry budgets and charged syscalls.
x86 kernel cpu_relax hooks also charge bounded reads with interrupts masked; arbitrary assembly loops do not.
The [client](../crates/seedbox-machine/guest/progress/progress.c) also replaces x86 RDRAND/RDSEED with guest entropy.

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
applies packaging adjustments and builds the launcher, core, drreg and dependencies.
The progress client still uses the matching release SDK.

Follow the [README setup](../README.md#try-it) after cloning or switching Seedbox
revisions. `inputs.self.submodules` includes gitlinks for flake consumers, but
Nix can lock a clean parent before noticing edits confined to a submodule.
From the checkout root, obtain a URL-encoded reference and build live DR edits:
`seedbox_flake=$(bash tools/dev-flake.sh) && nix build "$seedbox_flake#machinePvAssets"`.
For shell entry, see the README. Bare `.#machinePvAssets` can ignore those edits.
Direnv and the asset builder request submodules on the initial fetch too, using
the current checkout root, including linked Git worktrees and paths containing
spaces or URL syntax characters.
Nix traverses the nested elfutils, libipt and zlib gitlinks, so initialize all
three; drsyms remains excluded from the build.

Direnv's [helper](../tools/dev-flake.sh) reads NUL-delimited index gitlinks and
initializes each missing checkout separately, then visits its descendants.
Initialized checkouts are never updated, even when their HEAD differs from the
gitlink. To deliberately align all sources with a Seedbox revision, first save
local work, then run `git submodule update --init --recursive vendor/dynamorio`.

For coordinated edits, switch the submodule to `seedbox/release_11.3.0-1-backports`,
edit and verify it with Seedbox's PV asset build, then commit and push DR first.
Commit `vendor/dynamorio` in Seedbox to record the published DR commit and push
the Seedbox branch. Add new DR files to its index so local Nix builds include them.
The Mac builder exports tracked submodule files with local edits, excluding Git
metadata and ignored build output, to its Linux container.

## ARM idle rendezvous

Making an interrupt pending while idle has interrupts masked can slide delivery past a later unmask.
That changes saved kernel state and entropy even when application output looks identical.
The patched idle path unmasks before the stopped PV rendezvous, then restores the caller's mask on return.
The [kernel hook](../crates/seedbox-machine/guest/kernel/arm64/seedbox.c) lets the VMM assert the line at that exact boundary.

## Canonical saved vector state

XSAVE buffers can restore identical zero YMM state while differing in presence bits or ignored payload bytes.
Host interruptions can expose those differences in full-RAM comparison despite equivalent architectural state.
Kernel task-state saves canonicalize zero/absent YMM; direct application XSAVE and some signal paths remain outside it.
The [patch](../crates/seedbox-machine/guest/kernel/x86_64/apply.py) changes guest bytes, not the comparison's coverage.

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
