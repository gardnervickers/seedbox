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
generates the fixed `examples/faults` project with four writes per batch into the
evidence directory with `examples/faults/generate.py`, prepares it, and may build its workload images
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
undo margin and never reads `--margin`. In the PR #99 receipt, undo-off stepping
cost about 9x a default replay at 16384 (about 17x at 32768), so the margin remains
the profile's smallest
adaptive one. Replays use the retained engine/kernel without overrides. `result.json`,
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

## Work per host check

The baseline is the [PR #99 host receipt at 1e442c6](https://github.com/gardnervickers/seedbox/pull/99#issuecomment-6042505104),
read before this reduction. It reports a 5950X development-tier check with the
stored 52eb pinned reference and e8ece7cb kernel. Its step times are build 20 s,
VMM 11 s, fmt 0 s, clippy 9 s, portable suite 23 s, smoke 108 s: **171 s total**
(rounded steps). Smoke phases are fixed 13.7 s, default replay 9.5 s and
undo-margin replay 84.1 s. The latter is 78% of smoke and 49% of the whole check.
The older 119.8 s total in the PR description predates the final phase receipt.
The 768.6/334.9/1119.4 s records in `docs/acceptance.md` describe superseded
matrices; they are not timings for this three-execution tier.

| Candidate | Expected saving and evidence | Decision |
| --- | --- | --- |
| Reuse prepared OCI/initramfs across phases by digest | **0 additional preparations saved** in this tier: only `fixed` prepares; both replays already resolve the retained content-addressed closure. The wrong-kernel control refuses before preparation. The 13.7 minus 9.5 s difference is not a preparation timer. | Keep the public run/replay path. Native Podman already shares the three services' identical build/import/unpack source within one preparation. Their launch recipes include distinct IPs/commands, so guest archives are not interchangeable. No new cache. |
| Boot-fork / launch barrier | [PR #73](https://github.com/gardnervickers/seedbox/pull/73) measured 7.37 s cold versus 2.29 s forked for a historical four-machine fixture: **5.08 s per additional seed**, with 0.016 s restore. That is a lead, not a saving measured on this smoke fixture or its undo-off replay. **0 s claimed here.** | Defer: `--fork-seeds` is an in-process low-level VMM path, without public Compose run/replay wiring or cross-process snapshots. Adding a boot object changes seed/release semantics; substituting it would stop checking the current cold-boot path. |
| Shorter repeated workload | **224 fewer successful PUT/GET exchanges per execution, 672 across the three executions**: 128 PUTs + 128 GETs become 16 + 16, an 87.5% reduction of this repeated work. This also removes 112 sync-on-PUT operations per execution. | Implement four writes per batch in smoke only; keep the general example at 32. |
| Drop duplicate executions / source phases | **0 further executions safely removed.** PR #99 already combined undo and margin into one 16384 replay. Its earlier 32768 replay alone cost 165.3 s; plain undo-off exceeded the allocation. Default replay, doctor text/JSON, wrong-kernel refusal, and source checks each cover a different contract. Workspace tests occur once inside the portable step. | Keep all phases, `--undo 0 --margin 16384`, full state and witness comparisons. Do not skip retained-object hashing: it checks evidence integrity after each execution. |

For the shorter workload, the planning estimate is **7/8 of the time spent on
those repeated write/read cycles**, not 7/8 of the whole tier. If their original
combined cost across the three executions is W seconds, the first-order saving
is 0.875 W seconds. Neither #99 nor the existing phase timers isolates W; there
is no defensible point estimate in host seconds yet. Booting three machines,
Python startup, eight failed probes, fault delivery, timed waits, hashing and
cleanup remain. The undo-off phase is the likely largest beneficiary because
it accounts for most execution time, but the receipt does not distinguish boot
stepping from workload stepping. The extra portable controls also add a small
source-suite cost. No KVM host was contacted or run for this change.

The reduced fixture keeps four separately acknowledged batches across generations
1, 2, 2, 3, both stop/restart cycles, both overlapping partition handles, both
four-attempt failure probes, partial/final healing, readiness checks, and GET
verification of **every** acknowledgement. Healthy recovery reloads 4 then 12
records; the broken-server control loses 12 of 16, while broken-without-restarts
still passes. Portable tests run real TCP server/client processes for these three
cases and exercise the actual scenario with a fake World, including an ineffective
partition rejection. They do not establish KVM fault delivery or replay equality.
This preserves path coverage, not the old repeated-load volume or exact old
fixture witnesses: new workload arguments are retained in the input identity;
comparison rules, engine witness generation and full-state checks are unchanged.
The existing larger example and its acceptance matrix remain available for load
coverage.

On the next separately authorized host check, compare `fixed`, `default`,
`undo-margin`, total `smoke`, and `suite` against the above receipt with the same
engine, kernel, core and cache conditions. Require all existing outcomes,
full-state/opaque-stream comparisons and restoration/lease checks without retries.
Keep the new receipt even if it fails or is slower. Boot-fork equivalence, guest
execution of this shorter fixture, its actual saving, and five-minute fit remain
unverified until that check.
