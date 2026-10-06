# Local Mac TCG integration with the merged network fixture

2026-10-06. PR #16, implementing issue #10's reusable fixture, merged as
`4c7ed71aefc86d214644cd997512204293bdd205`. This worktree was checkpointed and
rebased onto that commit; the merge watch is stopped. Production KVM code is
unchanged. The initial recommendation for #8 is an **optional, restricted
single-kernel TCG profile**. Independent-machine Mac support remains a separate
probe, and the production backend selection is still open.

## What was integrated

`service.py` consumes the final merged initramfs and verifies its `single.init`,
`network.sh` and `members.sh` against the checkout. A canonical overlay seeds
Linux entropy before services start, loads stock-kernel networking modules, and
substitutes a guest-local FIFO for the fixture's COM2 command input. The original
namespace topology, directed cuts, child ownership/reaping, binaries and real
etcd client/checker remain in use. COM2 records requested and applied actions.

The fault scenario requests n1→n2 partition, heal, n2 process stop, and restart
at 0.7/1.4/1.8/2.4 seconds plus a seeded 0–100 ms offset after client startup.
Absolute guest CLOCK_MONOTONIC sleeps drive these requests. This is a new,
explicit epoch; KVM world timestamps cannot be reused as guest monotonic time.
No host wall-clock arrival chooses fault timing. Start acknowledgment means
launched; the real checker subsequently establishes service/data recovery.

The measured profile is QEMU 11.1.1, stock Linux 6.18.54, x86 TCG single-thread,
one vCPU, 1 GiB configured guest RAM, fixed RTC, `icount shift=0` (one virtual
nanosecond per instruction), and no external network or disk devices. Host:
Apple M4 Max / Mac16,6, macOS 27.0.1 / 26A434. These virtual nanoseconds are a
simulation policy, not calibrated native CPU time. Netem uses fixed 1 ms delay;
random jitter/loss is outside the qualification.

## Evidence and interpretation

The [audited result](evidence/services-2026-10-06/summary.json) links to retained
per-attempt inputs, source snapshots where available, console bytes, management
logs, exit codes and witness records. Failed development attempts are retained.
The aggregate budget includes their QEMU wall time; preparation and compilation
are excluded. Large initramfs and binaries remain in ignored local bundles.
All 37 service-phase attempts consumed 546.145 seconds of the 600-second budget.

Five fresh seed-1009 fault runs produced identical complete console, management
log and historical scoped witness. Those pre-fix witness records are provisional;
the corrected decoder has been qualified on two further seed-1009 fault runs
below. Each original run acknowledged 477 writes, recorded one failed
request, and passed the recovery checker at revision 479 / hash `42e55b68`.
The live partition dropped 12 packets. Median boot-to-exit wall time was
14.643 seconds (14.053–15.149). The client began at guest monotonic 3.516 s;
the bounded write phase lasted 3.006 guest seconds. Wall boot/setup phases and
peak host RSS were not measured separately; 1 GiB is configured RAM, not RSS.

Packet controls check namespace isolation, baseline versus 1 ms per-direction
delay, blocked delivery, reverse directed traffic, overlapping cuts/selective
heal, the client namespace, and delivery after healing. The healthy control
requires all 128 writes and no failures. The planted missing-write control must
produce application FAIL naming `k/99/000000`; an arbitrary error is rejected.
Additional seeds and the same seed set on etcd 3.5.3 are separate bounded
controls. Seeds 1010 and 1011 passed with distinct witnesses; etcd 3.5.3 passed
on all three seeds. Their application checks remain valid, but their historical
witnesses have not been rerun with the corrected decoder. Turning off witness collection preserved seed-1009 console,
management log and checker result. These small windows do not establish
historical bug-discovery rate or same-seed repeatability of the two singly run
additional seeds. The 12 portable harness tests also pass.

The witness hashes selected instruction dispatches (system calls/returns,
interrupt instructions, port I/O, timestamp reads, halt) and QEMU architectural
discontinuities (interrupts/exceptions/host calls), in execution order. Each
instruction record carries virtual PC and instruction-byte hash; discontinuity
records carry kind and source/destination PCs. Schema:
`seedbox.tcg.boundaries.v2`. It ends at the guest's explicit debug-exit OUT;
the harness also requires matching QEMU exit status and fixture completion.
This is a scoped execution witness, **not a full instruction trace, register,
RAM or device-state digest**, and it is not comparable to KVM witness hashes.
Instruction callbacks observe dispatch before completion. The opcode selector
is specific to this pinned x86 guest profile, not a general disassembler.

An earlier uncut witness varied by one or two events despite matching outcomes.
The final probe records 1–3 selected events after terminal I/O as diagnostics;
excluding that shutdown suffix gives exact repeatability through completion.
This supports the terminal-boundary explanation for the final witness drift.
It does not prove that every earlier translation-block witness difference had
the same cause. Both failed comparison batches and plugin bring-up failures
remain in the evidence. The early shift-3 healthy pilot completed only 115/128
writes before its guest deadline: its old `ERROR` label means INCOMPLETE under
the corrected classifier, not an application bug.

## Smallest demonstrated interface

| Boundary | Needed for this profile |
| --- | --- |
| Admit/create | Pin guest inputs, seed, kernel/device/time policy, topology and supported faults; reject unsupported requirements |
| Bootstrap | Seed entropy before processes, load a complete guest-local scenario, identify its time epoch |
| Execute/control | Run a bounded whole guest; guest virtual-time sleeps deliver commands through a reliable control transport |
| Observe/finish | Preserve requested/applied times, action failures, real checker outcome, explicit terminal boundary and declared witness schema |

Guest-local packets need no per-packet host interface. A shared `advance`
contract is still needed only if the independent-machine probe demonstrates
how TCG can stop, exchange timestamped frames and resume deterministically.
Do not force KVM's PMU mechanism or its execution witness into the TCG adapter.
This slice establishes requirements for #2's prepared-run/result interface; it
does not introduce a competing production manifest or change KVM scheduling.

Fault injection remains a capability contract. Shared-kernel process restart
preserves tmpfs data but cannot stand in for machine reboot, clock skew,
power-loss durability or independent-kernel failures. General fault handles,
in-flight packet policy, snapshots, multi-QEMU coordination and campaign
effectiveness remain explicit follow-ups. #10's merged research fixture does
not by itself close the project's final backend-selection gate.

## Review fixes and requalification

An Opus 5.5 review identified concrete weaknesses in terminal instruction
decoding, interrupted-process accounting, timestamp causality and evidence
validation. The decoder now examines the opcode and complete instruction length,
with portable byte-vector regressions. The process wrapper handles SIGINT,
SIGTERM and SIGHUP, kills/reaps the child process group, and atomically records
its elapsed time even on interruption. A hard crash leaves a pending receipt;
further runs refuse to start until it is reconciled. SIGKILL/power-loss cleanup
cannot be guaranteed by a user-space signal handler.

The evidence auditor now checks the raw console, management and witness files,
reclassifies qualified runs, requires real witnesses for sensitivity comparisons,
binds manifests to their runs and compared runtime/plugin identities, and rejects
orphan or duplicate attempts. Rejected witness bytes remain hashed and auditable
as failed attempts. Fault
logs must preserve causal timestamp and acknowledgment order; commands may still
queue while a previous action is running.

All 21 portable tests pass, including actual signal delivery to harmless test
subprocesses and tampered-evidence cases. Two fresh corrected-plugin fault runs
match the original console, management log and canonical witness exactly. The
[supplemental evidence](evidence/review-fixes-2026-10-06/summary.json) preserves
their exact sources and receipts alongside the original evidence. All 39 QEMU
attempts now total 573.504 seconds within the original 600-second budget. The
original five-run records are preserved rather than rewritten as new runs.
The full historical witness matrix remains provisional pending corrected-plugin
runs in a future bounded campaign. A small post-terminal event count is useful
diagnostic evidence, but is not sufficient to certify an old decoder's boundary.
The [resumed Opus 5.5 review](evidence/review-fixes-2026-10-06/claude-review.txt)
found no actionable issues remaining. Its session ID, reviewed source hashes and
independent verification are retained in the adjacent `review.json`.

## Reproduce with cached inputs

From this directory, use the pinned project research flake:

```sh
nix develop --command python3 -m unittest discover -p 'test_*.py' -v
nix develop --command python3 service.py --mode fault --shift 0 --witness \
  --seeds 1009 1009 1009 1009 1009 --timeout 60 --output runs/new-fault
```

Use `--mode controls`, `healthy`, or `negative` for controls, omit `--witness`
for instrumentation comparison, and use `--etcd-version 3.5.3` for the fixed
version. Output directories must be new. The durable
`build/service-budget.json` bounds aggregate QEMU time to 600 seconds; do not
reset it to hide earlier attempts. A future campaign needs a newly agreed
budget. Run one batch at a time. The witness uses macOS CommonCrypto and QEMU
plugin API 7; portability of that plugin to Linux is not implemented.

Required cached inputs (first preparation is not automated):

- `build/bzImage`: stock kernel SHA256
  `66111c2888091bd64e96e71a6a9e831ab5496d6c6272add5d81490396bfa3cdc`.
- `build/single-merged.cpio`: merged image SHA256
  `4a926d378fb651676a1c4f208fa0be0c13a098f5d94bffcbde1d6f55b1f61220`.
  Copied read-only from `/home/gvickers/seedbox-single-vm-network/artifacts/single.cpio`
  on the existing NixOS research host. Its fixture sources are checked before run.
- `build/modules/{veth,act_gact,sch_ingress,sch_netem,cls_u32,cls_matchall}.ko.xz`,
  copied read-only from Linux 6.18.54 modules in Nix output
  `/nix/store/wk649dz19ka48x7yd21gh8ynxxzjrihh-linux-6.18.54-modules`.
  All six have no module dependencies in that build; decompressed input hashes
  are recorded in every run manifest.
- Optional `build/etcd-3.5.3`, SHA256
  `79e672f61c4ae4197cee976fc4e6ab94cf42b40124a42e1c3774dc34c8d01c5e`,
  copied read-only from the retained `/home/gvickers/seedbox-sim/src/etcd/v3.5.3/etcd`.

The bootstrap is cross-compiled locally with Zig to static x86 Linux/musl;
the witness is compiled locally with Clang. Cached runs need no Linux builder.
Source was not uploaded to another host. Binary reproducibility across build
hosts and an automated clean-checkout artifact build remain unqualified.

One healthy attempt overlapped the planted-control batch during orchestration.
The budget was reconciled from the retained per-run durations, recovering that
attempt's overwritten ledger row; the evidence audit checks every retained
attempt against the ledger. The final runner adds an exclusive batch lock to
prevent recurrence. This host-only guard was added after the measured runs;
the exact executed runner snapshots remain alongside each final batch.
