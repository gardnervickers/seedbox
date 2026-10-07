# AMD admission checkpoint and deferred portability for #37

Research revision based on Seedbox `8a3e14ab2d9ded39082e1bc6c0f5ac531a454ce4`.
This branch contains procedures and observations; it changes no production code
and admits no new host. Conclusions and review decisions belong in
[issue #37](https://github.com/gardnervickers/seedbox/issues/37).

## Immediate decision

Finish engine/default-setting and application/fault acceptance on the existing
5950X first. A second Zen host is unidentified; the user confirms an additional
Intel machine is unavailable. Both live portability gates are HARDWARE_BLOCKED.
No cloud, purchase or provider experiment is authorized for this checkpoint.
Engine #33 owns the integrated candidate and scarce guest capacity. This lane
used zero guest seconds, no remote builds, no MSR writes and no CPU reservation.

The inventory collector has been executed on the existing workstation using the
project's offline Nix shell. It reads kernel/KVM/perf/topology information and
opens `/dev/kvm` for API and supported-CPUID queries only. It never creates a VM,
takes a lease or reads/writes an MSR. Its output is inventory, never admission.

`evidence/checks.json` and adjacent logs retain the check status. The required
portable repository checks passed. The Mac runner's 21 portable tests passed in
its own pinned shell after two environment-only failures: missing offline
dependencies, then `--max-jobs 0` preventing the local shell derivation. Cached
dependencies plus one bounded shell build resolved that prerequisite; neither
failed attempt reached a test or guest execution.

## Smallest current-platform admission recommendation

Use #33's reviewed integrated candidate/matrix and #36's retained coverage map;
do not start another AMD campaign for this lane. Cross-host and Intel work are
parked and cannot become prerequisites for useful single-host qualification.
The additional evidence needed now is limited to:

1. Resolve the known clock-bound and default PMU failures with causal evidence;
   qualify the exact corrected candidate at the ordinary default settings.
   An original ERROR followed by PASS/NOT_COMPARABLE is diagnostic evidence,
   not a qualified replay.
2. Fresh-process replay of the exact retained candidate/input/seed on this
   concrete AMD host, including the available full-state and margin/undo/
   observation comparisons. Preserve any default failure or unexplained drift.
3. The already agreed genuine application/fault/race and checker-control cases:
   use existing user-owned checkers and designated expected exit status, verify
   actual fault effects/recovery, and retain errors/incomplete attempts. A
   planted failure does not replace a genuinely discovered application failure.
4. Existing lifecycle/content-retention/lease tests plus exact MSR restoration.
   To claim the workaround was enabled during execution, capture a valid
   read-only sample while the executor's original-value snapshot and CPU/SMT
   leases are still held. External before/after values alone prove restoration,
   not the enabled interval. #33 owns this capture; its sampling proposal is
   not already implemented/tested evidence. Discard absent or racy samples.
5. Bind that receipt to this actual CPU/kernel/microcode, exact binaries/guest
   inputs and existing guest CPU/clock/entropy policy. State the tested runtime
   and instruction/clock paths; do not infer arbitrary Linux/JVM admission from
   the inventory. Supported CPUID is observed here; actual guest CPUID is not.

These checks protect deterministic replay, real application/fault behavior and
resource ownership on the first platform. They do not require a new output
schema, generalized admission product or backend abstraction. Any additional
#5 tooling needs a separately justified concrete gap in this existing matrix.
Collect boot/RSS/host-cost evidence opportunistically in candidate runs; it is
not a new correctness gate and configured RAM is not a residency measurement.

## Evidence scope

These labels state coverage rather than imply that one kind of evidence proves
every other capability. Finite tests qualify listed workloads and mechanisms,
not arbitrary Linux programs.

| Label | Evidence needed | What it does not establish |
| --- | --- | --- |
| DOCUMENTED_HYPOTHESIS | Primary documentation or source | An event, privilege, nested facility or backend works on a target host |
| PROCEDURE_VALIDATED | A recipe/tool exercised within its declared scope | Its guest tests ran, or its target host is qualified |
| OBSERVATIONS_MATCH | Fresh processes, retained inputs, raw observations and equality checks | Execution identity or hidden state equality |
| SCOPED_EXECUTION_MATCH | Exact declared witness schema, terminal boundary, fresh runs, result and fault-effect checks | Unobserved instructions/state; another witness schema or backend |
| KVM_STATE_MATCH | Execution comparison plus every configured machine's RAM/register digest and raw result/fault evidence | Complete device/world snapshots, durable storage, or arbitrary-workload admission |
| CROSS_HOST_MATCH | Two actual hosts, same retained bytes and candidate, self-replay and bidirectional cross-host comparisons, agreed profile controls | Other CPU models, host revisions, backends, runtimes or future builds |

Keep separate fields for capability, observation scope, tested versions/hosts,
status (DEMONSTRATED_WITHIN_SCOPE / DISPROVEN / UNRESOLVED), and the exact next
probe. Missing hardware is UNRESOLVED with blocker HARDWARE_BLOCKED and execution
status NOT_EXECUTED. A portable test pass cannot fill a guest-execution cell.

## Live platform observation

`evidence/initial-workstation.txt` and `evidence/workstation-inventory.json`
record the actual captures. The source transferred for the inventory was a
tracked `git archive` of the baseline commit, into the private directory
`/home/gvickers/seedbox-qualification37-20261006`. The archive has no `.git`, so
the inventory honestly retains the failed `git rev-parse`; source file SHA-256
values, the transfer command and this exact archive revision identify it.

- Bare-metal AMD Ryzen 9 5950X, family 25 / model 33 / stepping 0; 16 cores,
  32 threads, one socket; microcode `0xa20102e`; running Linux 6.18.54.
- Initial MemAvailable approximately 39 GiB; this is an instantaneous observation,
  not an allocation. `/dev/kvm` exists and the query returned KVM API version 12.
- KVM supported CPUID had 63 entries; canonical table SHA-256
  `e50b530be46c6276d080b7727f01bae9d9dcb6ba7d627c2f56e1e3e77cdf89a9`.
  This is **not** the table exposed by a running Seedbox guest.
- Observed `kvm.enable_pmu=Y`, `kvm_amd.nested=1`, `nmi_watchdog=1`,
  `perf_event_paranoid=2`. These readings do not establish nested counting or
  interrupt delivery. The collector did not open a perf event.
- The initial unprivileged lock snapshot found no Seedbox lease entries; it is
  not a reservation or proof that all locks are visible. Before any future guest,
  inspect live jobs and acquire the actual shared locks. Engine reserves 2/18.

## Already-prepared record/procedure reference (parked portability)

The remaining procedure was prepared before the scope reduction. Preserve it
for future access; it is **not the current V0 gate or a framework backlog**.
The current recommendation above takes priority. Do not implement new collectors,
formats, CPU-model abstractions or provider probes to complete this checkpoint.

The engine owner must provide an accepted, exact candidate and retained case set.
Do not select an older passing build to bypass an unresolved default-setting bug.
The historical Compose engine hashes in `docs/acceptance.md` precede the crate
relocation and are not the current checkout's binary. Never rebuild the VMM on B
and call it the same engine; copy the exact executable/content-store bytes.

For each candidate retain:

1. Exact CLI and VMM SHA-256, source revision, both Cargo lockfiles, pinned Nix
   inputs and build invocation. VMM builds use only `cargo build-vmm`.
2. Exact controlled kernel/config/patch, supervisor bytes, guest archives, images
   (platform manifest/config/layers), resolved commands/environment, controller
   program and configuration. Preserve the complete content-store closure.
3. Seed, topology/placement, memory per machine, kernel/control ABI, semantic
   schedule/network/limits, witness schema/mode and clock/entropy policies.
4. Each host's vendor/family/model/stepping, microcode, running kernel and booted
   build identity/config, KVM/perf capabilities/settings, PMU type/raw encoding,
   eligible core type and SMT topology, permission model and workaround readbacks.
5. **Actual guest-visible** CPUID/feature and clock/entropy probe observations.
   Include leaf/index/flags and every register in the CPUID table, XSAVE features,
   advertised cache/address-width information and relevant instruction behavior.
6. Per-attempt input closure verification, outcome/exit/stop reason, world and
   per-machine execution evidence, RAM/register digests, fault request/effect
   history and actual guest acknowledgments, raw logs, all errors and exit status.

Inventory fields such as hostname, CPU number, capture time and host cost provide
attribution; they do not enter the guest's semantic identity. A qualification
receipt binds both concrete inventories to an explicit compatibility class.
Start conservatively with the same CPU family/model/stepping, host kernel/build,
microcode, KVM/perf settings and realized guest table. Even that is a candidate
class requiring the matrix below. A differing field requires a deliberate
compatibility decision and rerun; merely assigning the same profile name does
not make the hosts compatible. Keep performance settings recorded per attempt
without turning them into a new semantic identity to conceal divergence.

The current `ExecutionProfile` records backend, engine, witness mode and semantic
parameters. Its compatibility checks are not a complete host-admission system.
Current guest CPUID starts from KVM's supported table and masks selected fields;
it does not supply a fully host-independent CPU model. `inspect --json` validates
the manifest checksum, not every content-store blob. Replay verifies its declared
input bytes before the backend runs. Rich inventory/drift enforcement is future
#5 work, outside this research branch.

Each proposed admission requirement enables a current functional dependency:
input/binary hashes bind the replay to the execution that failed; concrete host
and guest CPU controls bound counted progress and instruction/clock behavior;
default/performance/observation comparisons protect within-version determinism;
result/exit and fault-effect records establish completion and what actually
happened; retained bytes permit offline colleague replay; leases and exact MSR
restoration prevent interference and preserve host state. Hostname, timestamps
and costs are attribution, not new compatibility restrictions.

The JSON formats on this branch are disposable **research receipts**, not
external schemas, SDK conventions or production API proposals. They impose no
application-output/signature format, checker logic or workload coordination.
Application payloads, artifacts and their meanings remain user-owned.

## Parked cross-host admission procedure

Prerequisites: engine #33's integrated candidate passes default/performance tests;
the public-CLI case migration has a reviewed coverage inventory; A and B are two
accessible authorized physical hosts; artifacts and host controls are available.
A VM or another CPU on A is not B. Stop at a failed prerequisite and report the
exact missing evidence.

Use at least these admitted cases at the same exact candidate:

| Case | Seed/expected evidence | Purpose |
| --- | --- | --- |
| Controlled branch and clock/entropy probes | Fixed counted loops; seed 42 repeated and 43 sensitivity; guest-only host-work control; controlled clocks, RDTSC/RDTSCP, getrandom/AT_RANDOM and masked RNG instructions | Counter/filter/time/entropy admission, not merely application success |
| Real Redis service/workload | Retained seed 1009 PASS | Ordinary OCI/bootstrap/data behavior |
| Three etcd machines plus workload | Retained seed 1009 PASS | Go/lock-heavy behavior with independent service kernels |
| Fault/recovery scenario | Reviewed seed and actual partition/heal and stop/start acknowledgments | Fault effects, causal timing and real recovery |
| Planted checker FAIL | Retained control, FAIL/MATCH | Checker sensitivity and faithful reproduced failure classification |
| Genuine discovered failure and fixed-version control | Candidate-specific engine-owner record with exact versions/seed/evidence | A discovered application failure is not replaced by the planted control |
| Seeded race | At least two seeds with distinct lost-update counts; atomic control; repeated failing seed | Seed variation changes execution and replay preserves the failure |
| Semantic timeout and lifecycle controls | TIMEOUT/MATCH; wall timeout/cancellation NOT_COMPARABLE; contention rejection; exact MSR restoration | Distinguish semantics, infrastructure and host cleanup |

Missing candidate-specific genuine failure, clock/entropy or observation coverage
remains NOT_EXECUTED/UNRESOLVED; historical results do not silently fill those
cells. JVM/JIT, arbitrary instruction paths, reboot, disks/durability, full
world snapshots, debugger/reverse-step observation and guest-local netem jitter
are outside this admission unless separately tested.

The listed applications and controls are the qualification corpus for the
approved Seedbox bring-up. Their existing checkers establish the test's expected
behavior; they do not prescribe how other users write applications, represent
histories or interpret custom outputs. Retaining raw output for audit does not
make its payload semantics a platform contract.

For every completed semantic case originate one default full-state baseline on
A and retain it. Perform two fresh default self-replays on A, two A-origin
replays on B, two B self-replays of its first transferred-input execution, then
two replays of that B-created record on A. All use exactly the same input bytes,
seed, engine and semantic profile, with fresh process/output state. B's record
is created from A's retained inputs, not rebuilt images. This tests both replay
directions and each host's repeatability; it does not test cross-build equality.

On each host replay the same baseline once with `--undo 0`, once with `--undo 16`
and once with `--margin 32768` (the default is adaptive / undo 256). Any failure
or unexplained difference is retained and blocks that case; completing only with
a larger margin is not default acceptance. No automatic retry or majority vote.
The first raw failure remains part of the receipt after a hypothesis-driven fix.

Observation variants require an exit-only/full-state/trace comparison over
identical retained guest inputs, seed and semantics. The public replay CLI
inherits witness mode and has no trace toggle; it cannot currently express that
whole matrix. The engine owner must supply a supervised, reviewed fixture for
these cells. Compare common exit/branch evidence when schemas differ and every
available state digest; do not compare unlike hash schemas or claim an omitted
cell passed. A debugger observation claim requires future debugger tests.

Acceptance requires MATCH plus matching result/stop reason, execution witnesses,
every machine's full-state digest and complete fault-effect evidence. Verify
machine key sets and inspect raw per-machine reports. Selected console hashes
alone are insufficient. End-state RAM/register hashes do not prove complete
device/world state or every interior instruction. A field that differs cannot
be silently dropped from comparison; diagnose its source and decide whether the
compatibility class or instrumentation must change before rerunning.

All cases carry explicit NOT_STARTED/RUNNING/PASS/FAIL/UNRESOLVED status and raw
attempt references. Finish the actual case set within a separately agreed budget;
sample counts support a bounded profile, not a mathematical universal guarantee.

## Executable capture and replay procedure

The collector needs Python from the root project shell and native OS inventory
commands; unavailable fields are recorded as errors. Save new filenames.

```sh
nix develop --offline --command python3 \
  experiments/backend-qualification-37/probes/inventory.py \
  --checkout "$PWD" > /absolute/new-output/host-before.json
```

After candidate acceptance, transfer the **retained state** (objects plus selected
run directories) and exact CLI to B. Record archive/file SHA-256 before and after
transfer. Copy to a private destination, verify manifest and referenced blobs,
and keep registries/builders/original checkout unavailable during replay. Never
overwrite either host's existing state. The package, not a mutable registry tag,
supplies the kernel, guest inputs and VMM.

For one default cell, with an agreed free physical core and sibling reserved:

```sh
# Absolute values come from the candidate receipt and approved allocation.
SEEDBOX_BIN=/absolute/candidate/seedbox
SEEDBOX_CASE=/absolute/private-state/runs/retained-attempt
SEEDBOX_STATE_DIR=/absolute/private-state
SEEDBOX_TEST_CPU=4
SEEDBOX_CELL_OUT=/absolute/new-output/a-self-default-1
mkdir "$SEEDBOX_CELL_OUT"
set +e
sudo -n "$SEEDBOX_BIN" replay "$SEEDBOX_CASE" \
  --state "$SEEDBOX_STATE_DIR" --cpu "$SEEDBOX_TEST_CPU" \
  > "$SEEDBOX_CELL_OUT/stdout" 2> "$SEEDBOX_CELL_OUT/stderr"
SEEDBOX_CELL_RC=$?
set -e
printf '%s\n' "$SEEDBOX_CELL_RC" > "$SEEDBOX_CELL_OUT/exit-status"
# Resolve exactly one OUTCOME line and inspect its fresh record --json.
# Save the record, all raw logs, pre/post inventory and MSR readbacks.
# Repeat sequentially with fresh cell labels; add explicit performance flags
# only for their named cells. Never pass --allow-engine-change in this matrix.
```

CPU 4 above is illustrative, never a reservation; inspect jobs/topology first.
Current CPU 2/18 belongs to engine diagnosis. Every guest entrypoint must acquire
the shared nonblocking `/tmp/seedbox-msr-cpu-N.lock` locks for the core and all SMT
siblings in ascending order, with no symlink following and descriptors held
through process reap and restoration. The current public executor does this.
Do not add outer identical locks around that executor: it owns its own locks.

Before/after readbacks of MSR `0xc0011020` on each leased Zen sibling must equal
the exact originals, including failure/timeout/cancellation. The workaround is
bit 54; do not assume it started clear. Capture readbacks inside the supervised
lease window. Preserve a failed restore snapshot and classify infrastructure
failure. Neither root nor an idle lock snapshot licenses an occupied core.

Start any later campaign with one guest job/core per host and the SMT sibling
reserved, builds at most two jobs, explicit memory/headroom and at most one
allocated core-hour before a checkpoint. A four-service case configured at
1024 MiB each has 4 GiB guest RAM before undo shadows and preparation overhead;
this is an allocation estimate, not a measured RSS claim. The initial canary
should be the full-state Redis default/self-replay pair, then proceed only after
inspect/readbacks succeed. End the campaign immediately on unexplained drift.

## Falsifiable Intel and nested probes (not executed)

Keep Intel parked. Smallest future access is one named authorized x86 Intel host
with identified family/model/stepping and core type, `/dev/kvm`, ability to run
the test under its actual perf permission policy, an uncontended core/SMT lease,
and host/hypervisor configuration visibility. An exact model's conditional-branch
event encoding and errata must be selected before any counter is opened. Do not
run the current Zen-only VMM/workaround on Intel as a portability probe.

| Probe and hypothesis | Falsifier / required observation | Consequence |
| --- | --- | --- |
| Raw conditional-branch event can be pinned | perf_event_open using exact model/core-specific encoding and pinned event; raw errno, time_enabled/time_running; read never EOF; zero/multiplexed/unsupported count is not success | Unsupported event or core becomes ineligible; permission denial is a separate access blocker |
| Host work is excluded, guest kernel/user included | Same known assembly branch stream around controlled VM exits, vary only host work; exact N/2N counts and host-only zero control; separate kernel and user paths | Count change with host work or dropped guest paths disproves required semantics |
| Overflow promptly interrupts KVM_RUN and exact target is recoverable | Record armed period, signals/EINTR, raw counter and guest RIP/branch position at interruption; vary periods, syscall boundaries, undo/margins and host load | Lost overflow or uncorrected target overshoot blocks admission; a skid histogram alone is not a correctness proof |
| Observation/performance changes preserve execution | Identical admitted service/probe inputs, exits plus full state, defaults versus controls | Any unexplained divergence blocks support |
| Rootless execution is viable | Repeat event/filter/overflow probes as target UID under observed policy, then under already authorized privilege; retain errno and capability context | Privileged-only success refutes rootless use on that configuration; do not change host policy to make it pass |
| Nested execution provides required facilities | Inventory L0/L1 separately, realized CPUID and PMU at L1; create/observe L2 under a bounded authorized probe; run the identical guest-filter/overflow/exact-stop tests from L1 | A nested flag, visible CPUID PMU or KVM boot alone does not qualify it; lacking L0 visibility is unresolved |

The existing VMM uses a **host perf event** to count guest execution. It does not
need an in-guest vPMU merely to boot the guest. Under nesting, the VMM itself runs
in L1 and depends on what L0 virtualizes for L1's perf; these are separate gates.
Root can fix a permission boundary but cannot create an absent virtual PMU.

Primary sources bound these hypotheses:

- [perf_event_open](https://man7.org/linux/man-pages/man2/perf_event_open.2.html)
  defines pinned/exclude_host, counts and overflow sampling. `precise_ip` concerns
  sampled instruction pointers; it does not promise exact Seedbox guest stops.
- [Linux perf security](https://www.kernel.org/doc/html/latest/admin-guide/perf-security.html)
  distinguishes perf_event_paranoid, CAP_PERFMON and root. No capability or sysctl
  changes are part of this proposal.
- [Intel Broadwell events](https://perfmon-events.intel.com/platforms/broadwell/core-events/core/)
  and [Arrow Lake E-core events](https://perfmon-events.intel.com/platforms/arrowlake/core-events/e-core/)
  show that conditional-event encodings and restrictions depend on the CPU/core.
  These examples are not an encoding choice for an unidentified target.
- [KVM nested guests](https://www.kernel.org/doc/html/latest/virt/kvm/x86/running-nested-guests.html)
  explains L0/L1/L2 and required configuration; it gives no Seedbox branch/skid
  qualification.
- [KVM API](https://www.kernel.org/doc/html/latest/virt/kvm/api.html)
  documents supported CPUID and PMU virtualization controls. A supported-CPUID
  query is not a guest-execution observation.

## Mac TCG assessment and useful cost evidence

Historical evidence is at immutable commit
`788fc4376b7e2cc96d1bc51ed2ae3d4635d16abd`, particularly
`research/mac-tcg/README.md`, `SERVICE_REPORT.md` and their linked raw captures.
The active tools live under `tools/mac-tcg/`; their portable tests check the
runner/evidence handling and do not requalify guest execution.

Historical artifact pins: stock kernel SHA-256
`66111c2888091bd64e96e71a6a9e831ab5496d6c6272add5d81490396bfa3cdc`;
service archive
`4a926d378fb651676a1c4f208fa0be0c13a098f5d94bffcbde1d6f55b1f61220`;
optional etcd 3.5.3 executable
`79e672f61c4ae4197cee976fc4e6ab94cf42b40124a42e1c3774dc34c8d01c5e`.
The active runner pins and retains QEMU/firmware/plugin/source hashes as well;
these three hashes alone do not reconstruct its execution profile. The current
KVM candidate artifact set is not yet supplied or admitted by this lane.

| Capability | Evidence/status | Next falsifiable probe and consequence |
| --- | --- | --- |
| Small local clock/entropy probe | DEMONSTRATED_WITHIN_SCOPE: nine fresh processes over seeds 42/43/44 repeat console/observations within seed; omission controls expose failures | Current cached-input repeat/negative-control campaign would refresh observations only |
| Single-kernel etcd faults/recovery | DEMONSTRATED_WITHIN_SCOPE: namespace controls, directed partition/heal, process stop/start, recovery and planted failure | A current complete witness/full-state campaign is required before extending execution claims |
| Corrected scoped witness | Two corrected-decoder seed-1009 fault runs match console/mgmt/selected-boundary hashes | Full corrected matrix, richer state coverage and observation controls remain unexecuted |
| Independent real machines | UNRESOLVED / NOT_EXECUTED: service fixture shares one kernel/clock/tmpfs | Two QEMU processes, controlled timestamped packet exchange, causal bounds and repeated state evidence; reject host-wall packet timing |
| Current private KVM PV ABI in stock QEMU | Unsupported by the existing tool/profile | A diagnostic PV boot can falsify direct reuse; any ABI/backend design needs a later reviewed research decision |
| Full-state replay/snapshots | UNRESOLVED; selected plugin hashes omit general RAM/register/device state | Capture and compare complete declared state with restoration tests; current plugin cannot fill this cell |
| Production run/replay and clean setup | UNRESOLVED: manual cached artifacts, separate research receipts and macOS-only plugin hashing | Fresh checkout/input-preparation plus ordinary retained-input offline replay; no production design commitment now |
| KVM/TCG execution equality | UNRESOLVED and not required for local feasibility | Distinct time/progress/kernel/witness policies prevent comparing current hashes; a common reference would be new research |

The Mac input probe measured Apple M4 Max / Mac16,6, macOS 27.0.1 / 26A434,
QEMU 11.1.1 and stock Linux 6.18.54. Guest RAM was configured at 256 MiB;
median **boot-to-exit** was 2.992 seconds (2.977–3.156). The nine-run batch's
child-process maximum RSS was 224,657,408 bytes (~214 MiB), not whole-machine
memory or guest RAM usage. The single-kernel service fixture configured 1 GiB;
five historical pre-decoder-fix fault repeats had median boot-to-exit 14.643 s.
Separate service boot/setup time and peak RSS were not measured. The two corrected
runs took 14.019 and 13.341 s. All 39 service attempts consumed 573.504 QEMU wall
seconds, including development failures; preparation/build time was excluded.

These results justify preserving the restricted local experiment. They do not
estimate current multi-VM KVM performance, snapshot benefit, seeds/hour or genuine
bug-discovery rate. No new Mac guest campaign ran here: ignored cached inputs are
absent in both the primary checkout and this isolated worktree, and the historical
service budget must not be reset.

[QEMU icount](https://www.qemu.org/docs/master/devel/tcg-icount.html) offers
instruction-derived virtual time in system emulation and requires single-thread
TCG. [QEMU record/replay](https://www.qemu.org/docs/master/system/replay.html)
can retain nondeterministic inputs for one emulated machine; this does not prove
Seedbox's coordinated, seeded multi-machine exploration. The existing experiment
uses fixed shift/sleep-off and fresh execution without a recording log.

## Review and next boundary

Submit this experiment revision and #37 findings to the standing reviewer. For
the design-only final gate, use the issue-comment URL and exact branch SHA; the
reviewer requests the manager's independent expectations before giving final OK.
Do not merge the branch or implement backend abstractions from this proposal.

Immediate follow-up belongs to engine #33's existing integrated matrix: complete
the AMD default/performance/observation and substantive application/fault evidence.
Further #5 enforcement gets a separate scope only for a concrete current gap.
Park the cross-host,
Intel and independent-machine Mac probes until their access and decision gates
are reopened. New access alone does not authorize spending or change the matrix.
