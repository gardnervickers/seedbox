# Acceptance record

Historical paths and commands below are as recorded, before the repository moved to a Cargo
workspace (`sim/seedbox` is now `crates/seedbox`, `sim/vmm` is `crates/seedbox-vmm`,
and the runners are under `harness/`). The engine hashes below are for builds
from that layout; a VMM built from `crates/seedbox-vmm` embeds different source
paths and is a new engine, even though its source is unchanged. Entries that
mention the counter timebase, `--margin`, `--undo` or MSR restoration predate
the removal of counter mode; those settings no longer exist, and runs recorded
with them do not replay on the current engine.

## 2026-10-08 — recorded guest epoch and unprivileged namespaces

Based on main `02971ad`, this follow-up retains one preparation-time Unix epoch
in `PreparedInputs.wall_clock_epoch` and `SBXLAUNCH4`. Explicit `--build-epoch`
sets it instead. The shared supervisor sets `CLOCK_REALTIME` before the launch
barrier. Historical inputs omit the new field without changing their encoding.
The image is recursively bound and moved over the mount namespace root before
the supervisor enters it: a plain chroot prevents Linux from creating user
namespaces. Existing bootstrap handles and named-volume submounts survive this
transition.

On the Mac, fmt, strict all-target seedbox clippy, workspace tests (632 passed,
5 ignored) and portable acceptance passed. Preparation tests cover a common
epoch across services, explicit zero, retained serialization and historical
omission; run/replay tests cover absent and recorded epochs without preparation.

AMD host checks used the specified fallback SSH identity, a private worktree,
the project Nix shell, and CPUs 14/15/30/31. Build/artifact directories and the
Podman store were task-owned temporary directories. The x86 kernel rebuilt and
passed `kernel/build.sh`'s config checks, including `CONFIG_USER_NS=y`. The ARM
application kernel cross-build used the project asset shell plus its Nixpkgs
cross compiler: a temporary build-script copy replaced only the native-host
restriction and selected ARM objdump for the existing audit. All 108 config
checks and the kernel/vDSO hardware counter/RNG audit passed. Native ARM/HVF
execution remains untested.

| Kernel | SHA-256 |
| --- | --- |
| x86 bzImage | `ce730ee5686b60168949f26cea5bc7d925b08cb4cdd36622c72debee56547232` |
| ARM Image | `4253d5df94c73b8bc4738af6d22fc0ba25bb7902366b3ab6bc41d1908bea875d` |

The static x86 supervisor and release CLI built; the ARM supervisor translation
unit cross-compiled with `-Wall -Wextra -Werror`. Standalone Linux VMM build and
`cargo test-vmm --features kvm-tests` passed (90 passed, 37 explicitly ignored).
These Linux checks cannot run natively on this Mac.

Both projects ran with the counter timebase, seed 172, 1024 MiB and full state.
Both replayed to **MATCH for outcome, witness, full state and fault history**:

| Project | Recorded epoch | World witness |
| --- | --- | --- |
| `harness/acceptance/workloads/runtime.yaml` | 1791512639 | `101b3d16a2ba5a802de7be23c4e43791b2ea36d26a979035a6d4ebd1bad737a8` |
| `harness/acceptance/workloads/runtime-tls.yaml` | 1791512655 | `5961617b588f756ace989f0fd98bca8ddd0fbce7258c3cb90717a6f616167102` |

The runtime probe passed as UID/GID 123/456, including a fresh user namespace,
UID/GID maps, denied setgroups and nested mount/PID/network/IPC/UTS namespaces.
Its existing named-volume write and device/IPC/limit checks also passed. This
covers Chromium-style namespace primitives, not a full Chromium application.
The TLS image minted its localhost certificate in the fresh preparation build;
the guest checked its issuance was less than a day ago, trusted it with normal
certificate/date/hostname validation and completed a client/server handshake.
Replay required no current clock, certificate generation or image preparation.

Engine SHA-256: `3a3cc80b8628f980d0c1b72e1ea4d998d38a8e5d799b8269fdec50043fd8cbc8`.
All four final MSR readbacks matched the initial snapshot, including after the
failed chroot diagnostic runs. Task-owned host worktrees, stores, runs, build
and artifact directories were removed; shared engine/kernel directories were
left intact. These scoped checks do not replace the final merge-train campaign.

## 2026-10-08 — issue #172 guest runtime batch

The Linux AMD/KVM workstation ran PR #178 at `93ef5a1` (on main `b90a315`)
from a clean checkout. The x86 kernel build used fresh `SEEDBOX_KBUILD` and
`SEEDBOX_ARTIFACTS` directories. Against main's kernel, `kernel.config` gains
only `SYSVIPC`, `SYSVIPC_SYSCTL`, `POSIX_MQUEUE`, `POSIX_MQUEUE_SYSCTL`,
`SECCOMP`, `SECCOMP_FILTER`, `UTS_NS`, `IPC_NS` and `PID_NS`; `IO_URING` and
`HPET_TIMER` are `y` on both.

- Kernel SHA-256: `2ee31d831d2409f19de813acb31192c1e1352559e1276e9d76b29a19a388aca7`
  (main: `d628d375dc79e65b2d080d8b04e93e65dc66a6f29d873272784554ab03d31824`).
- Linked CLI engine: `42b7ed58eaec4f76…`; the standalone VMM is unchanged
  from main (`35358821bdbd…`).

All cases used the public `seedbox run`, seed 172 and the default counter
timebase. `harness/acceptance/workloads/runtime.yaml` passed with 1024 MiB and
`--full-state` as UID/GID 123/456. It exercised private devices and fd links,
pty I/O, POSIX shared memory/semaphores/mqueue, SysV shared memory, service
hostname lookup, the 1048576 nofile limits (including fd 65535),
`vm.max_map_count=262144`, named-volume writes, an io_uring NOP completion and
a seccomp allow filter.

`seedbox replay` of that retained run reported `MATCH`, with outcome, witness,
full state and fault history all matching. World witness:
`846b2ef6d6c8de7b…`. On main, the same project fails at the `/dev/shm`
mode check and `runtime-postgres.yaml` fails with no server response.

The five `runtime-*.yaml` application projects passed with 2048 MiB:

| Project | Guest check | Resolved Linux amd64 manifest SHA-256 |
| --- | --- | --- |
| `runtime-postgres.yaml` | Postgres 16 initdb, query yielding 42, POSIX DSM selection | `1a66d744c1b459e13b05a8fca341da84cb63383e99ce262210efee5a319d4551` |
| `runtime-nginx.yaml` | nginx HTTP 200 and the access log in the retained console | `8f84ed99befc3891b8f329c5c202785278a2cfb7c25107d57fb2a134a3117433` |
| `runtime-python.yaml` | Python 3.12 non-root spawn-based ProcessPoolExecutor | `2055081c860db8c842b663415b795b026e7e5cc63b59c5193fcc9997713ea5b8` |
| `runtime-jvm.yaml` | Temurin 21 InetAddress.getLocalHost() resolves `jvm` | `9ececb44042a7cc91e4e6bca525c678e811c1602330a80b5f2d2fcfa0aa97c4e` |
| `runtime-node.yaml` | Node 22 local HTTP request/response and hostname | `2c752226d477b4a886378baa95b9af252be59301b725fdb0b7e15208131505a8` |

`examples/etcd` passed and replayed to `MATCH` on both revisions, with
different witnesses as expected. The standalone Linux VMM build passed; `cargo test-vmm --features
kvm-tests` passed 89 tests, with 36 explicitly ignored exclusive-host cases.
The supervisor and ARM I/O descriptor lifetime tests also passed on Linux.

This is scoped same-host evidence, before the final merge train. The full
host campaign and native ARM rebuild/HVF execution remain outstanding. TLS with a certificate minted that day needs the recorded
wall-clock epoch follow-up; this batch leaves the boot epoch unchanged. Node
HTTP alone does not establish the libuv io_uring path; the separate kernel
probe proves one ring completion.

## 2026-10-06 — OCI import and Linux compilation

Host: the supported Linux amd64 workstation, `nixos.local`. Tests ran in the
private checkout `/home/gvickers/seedbox-compose-pr17`, using `nix develop ./sim`.
No existing simulation checkout or runtime artifacts were modified.

`bash sim/seedbox/tests/accept-oci.sh` passed:

- Static `compose-init.c` compiled with `-Wall -Wextra -Werror` using the flake's
  musl compiler.
- The Linux VMM and Rust OCI/execution crate compiled. The portable tests and
  `cargo clippy --all-targets -- -D warnings` passed on Linux.
- The registry test verified and unpacked both real images below through the
  common skopeo/umoci import path, checked their executable files, and verified
  every retained layer.
- The Dockerfile test built and imported a real image, exercised whiteout
  deletion, a symlink, `.dockerignore`, and image command/environment/user/workdir
  metadata, and retained the build context.

| Image reference | Resolved Linux amd64 manifest SHA-256 |
| --- | --- |
| `docker.io/library/redis:7.2` | `6510e071eecd994fb187c15f126d424a293006669f590da800ca4a06564ae7d0` |
| `quay.io/coreos/etcd:v3.5.2` | `a89a3ffe8ad7b3d5d08b16bb7f33d9c5fe6a740678e7f9af2ca9915fa1366490` |

The first Dockerfile attempt failed because Podman had no unqualified registry
search list for `busybox:1.37`. The fixture and generated Dockerfiles now use
explicit `docker.io/library/...` references; the repeated test passed.

Release builds of both Rust binaries subsequently passed. Template tests also
passed on Linux: generated projects validate, retain their own workload source
and build definitions, preserve the existing etcd consistency checker, and refuse
to overwrite an existing project. Redis protocol checks cover binary bulk values,
nil versus empty values, invalid/error responses, and command argument boundaries.

These checks did **not** boot guests, change CPU affinity or MSRs, establish image
compatibility with the controlled kernel, or verify real run/replay determinism.
The subsequent KVM results are recorded separately below.

## 2026-10-06 — real KVM execution and lifecycle acceptance

The subsequent KVM suites passed on the same workstation/private checkout.
They used the existing controlled kernel without modifying its build or other
simulation directories:

- Kernel SHA-256: `01f9c903a4754bfe9eca4c197cc3335ab8ff582944164982463518128639e0d8`.
- Engine SHA-256: `a142e294b2d00dbd1573bc9865f59202618d3fe83f4ea0a160d2485ed31e39d9`.
- Guest ABI `oci-init-v1`, 1024 MiB per service, seed 1009.
- Normal/failure suite: `acceptance-20261006-3`, CPU 1 and SMT sibling 17.
- Lifecycle suite: `acceptance-20261006-5`, CPU 15 and SMT sibling 31.
- Full retained manifests, content stores, logs and readbacks are under
  `/home/gvickers/seedbox-compose-pr17/` in those two directories.

| Case | Actual result | Fresh replay |
| --- | --- | --- |
| Redis registry image + Rust workload | PASS, 32 acknowledged SET/GET pairs | PASS / MATCH |
| Three etcd registry images + existing Go checker in a Dockerfile image | PASS, 160 acknowledged writes verified | PASS / MATCH |
| Redis planted missing-value assertion | FAIL | FAIL / MATCH |
| etcd planted missing acknowledgement | FAIL | FAIL / MATCH |
| Redis with full-state digests | PASS, both machine digests present | MATCH, `full_state_matches: true` |
| Same full-state run with margin changed from `adaptive` to `10000` | PASS | MATCH, including both state digests |
| 1 ms simulated limit | TIMEOUT | TIMEOUT / MATCH |
| 1 ms host wall limit | INCOMPLETE / WALL_TIME_LIMIT | INCOMPLETE / NOT_COMPARABLE |
| SIGTERM sent to CLI after observing its live VMM child | INCOMPLETE / CANCELLED | NOT_COMPARABLE |

Selected execution witnesses (baseline and replay identical):

| Case | SHA-256 |
| --- | --- |
| Redis PASS | `016129e5e43515cdad6be343718cb462b4952f80364fae8b1eda2bce089501c7` |
| etcd PASS | `2136f2ea5b37205daa33436f9cdc742923891e05ab4454d3f65a7c32a46b48ac` |
| Redis FAIL | `77ad9bc00cb122b74e01a0f93121000951f03f8a3e5cf6bde1ec68e3e76d4b1d` |
| etcd FAIL | `97ca79b9eae8cb6de0b5fe04b51a6dc12e6c838429ba06fbb123853d3a7e43e8` |
| Redis full-state and changed-margin replay | `2c6144969dbe52be10b7107470fe0d082da6dd72f5b1c8ffcc11f09844d986de` |

The full-state machine digests were
`client=5140b076834c4006a6aa0fe01d0bd8d20ef52b15c149f80db03d3cc4f5d58521`
and `redis=86aeb8f3751f756973bb38721f7c908472c64f5562af35c6b30412f527d051db`.
They matched both ordinary replay and changed-margin replay. Different fresh
builds can produce different image identities; only matching retained inputs are
claimed to replay exactly.

Each script compared real MSR readbacks after every execution, including failures,
timeouts and cancellation, and rejected any leftover restoration snapshot. CPUs
1/17 and 15/31 all read `0x4480000000000` before and after their suites.
Two attempts on busy lanes failed with lease contention before guest execution
(CPU 2 in `acceptance-20261006-1`, CPU 1 in `acceptance-20261006-4`); both left
MSRs unchanged. The executor did not take over another runner's reservation.

The initial 512 MiB etcd attempt in `acceptance-20261006-2` failed while unpacking
the image (`Initramfs unpacking failed: write error`; `No space left on device`).
It was classified ERROR, not an application failure. Redis at that size passed
and replayed. The CLI now defaults to 1024 MiB, and preparation rejects profiles
below a conservative archive-size boot floor before acquiring a host lease.
This floor does not estimate application heap requirements.

These results qualify the listed examples and lifecycle paths. They do not
qualify arbitrary OCI images, whole-machine persistent-storage recovery, the
legacy seeded fault hunts, or a broad seed sweep. Nonempty Compose fault schedules
remain explicitly unsupported pending #3. The etcd template uses a fixed write
target rather than the legacy campaign's timed load; no throughput or bug-finding
rate equivalence is claimed.

## Final integration with main (`e344ce4`)

After the canonical examples moved to `sim/examples/`, the PR integrated main's
PV/undo VMM work and engine acceptance harness. Both Compose suites passed again
with engine SHA-256
`a55ecd4a2d3b98f9a575e461695aebabcfe22a5e36d1572bbb3ce59a558289b8`,
the same controlled kernel, and CPUs 15/31:

- `acceptance-20261006-6`: Redis and etcd PASS plus intentional FAIL controls;
  all four fresh replays MATCH. Their execution witnesses match the corresponding
  rows above as well.
- `acceptance-20261006-7`: full-state baseline and replay; margin 10000; undo 0
  and 16 against the explicit default 256; simulated timeout and replay; wall
  expiry and replay; and CLI SIGTERM while its VMM child was live. All expected
  outcome/comparison assertions passed. Every completed full-state variant had
  the identical witness `e8c6fc52c440d40143e410ad09bfdcd11437a87463d045b24d5c43782fa0f27a`
  and identical digests for both machines. This is a fresh prepared baseline,
  not a full-state comparison across engine versions.
- Both suites read `0x4480000000000` on CPUs 15 and 31 before and after each run.
  No restoration snapshot remained.
- `preflight-20261006-1`: preparing the checked-in etcd Compose definition at
  512 MiB returned ERROR before execution, explaining the 655 MiB archive boot
  floor. No prepared result was published.

The merged branch passed `sim/acceptance.sh portable` on Linux and Darwin
(the Linux-only VMM test step is explicitly skipped on Darwin), all 53 normal
Rust tests plus the doctest, formatting and Clippy with warnings denied. The
existing etcd checker's `go test ./...` passed in its new canonical directory.
The two rootful OCI tests had passed separately earlier; they remain explicitly
ignored in the ordinary portable test invocation. The legacy engine smoke/full
fault matrix was not rerun as part of this Compose acceptance.

## 2026-10-06 — initial YAML preparation migration (superseded)

This records the earlier PR #27 implementation. The guest-driver migration
below replaces its test-only preparation bridge and batch execution path.

The full Python acceptance suite passed using the YAML workloads in
`sim/acceptance/workloads/`, prepared by the test-only `acceptance_prepare` target.
The production crate, CLI, VMM, generic guest supervisor and batch supervisor
were unchanged. Source revision: `3406e1e`.

The run used `nix develop ./sim` on `nixos.local`, CPUs 2–7 with SMT siblings
18–23, the controlled kernel with SHA-256
`01f9c903a4754bfe9eca4c197cc3335ab8ff582944164982463518128639e0d8`, and VMM
`a55ecd4a2d3b98f9a575e461695aebabcfe22a5e36d1572bbb3ce59a558289b8`.
Evidence is retained under `/home/gvickers/seedbox-yaml-acceptance/runs/acceptance-full-20261006T192231Z-2621786/`.

- Portable checks passed on Darwin and Linux; all-target Clippy passed on both.
- Fresh exit and full-state replay matched, including every machine digest.
- Undo 0/16 and margin 32768 preserved the witness without retries.
- Four race seeds produced distinct lost-update counts; the atomic control was
  clean, and a fresh replay matched the sampled race failure.
- The intentionally planted missing acknowledgement produced the expected failure.
- Both scheduled short-workload kill/restarts were acknowledged; the workload recovered.
- All 11 etcd 3.5.3 seeds (4000–4010) passed with acknowledged recovery.
- Cross-build equivalence was skipped because this changes workload setup, not
  engine performance. No historical failure seed or witness was required.

The full tier completed in 768.6 seconds. All ten batches verified restoration;
final MSR readbacks on every selected CPU and sibling matched the initial
`0x4480000000000`. No restoration snapshots remained.

After rebasing onto the workspace restructure (`926060d`), source `184abf5`
passed fresh YAML preparation and the full smoke tier in 334.9 seconds. Evidence
is retained under `/home/gvickers/seedbox-yaml-workspace/runs/acceptance-smoke-20261006T193802Z-2640154/`.
All eight batches restored their snapshots, and final MSR readbacks matched the
same baseline on CPUs 2–7 and 18–23 with no snapshots remaining. The original
full-tier record above predates the layout change; at that revision workloads lived under
`harness/acceptance/workloads/` and the helper under `crates/seedbox/tests/`.

Workspace tests and portable checks passed on both hosts after the rebase.
Linux all-target Clippy passed with warnings denied. Darwin's strict check
encountered main's existing `HostLease::try_acquire_at` dead-code warning;
all-target Clippy passed with only the `dead_code` lint allowed. Production
source was left unchanged. Independent review passed after the path/API updates.

## 2026-10-06 — guest drivers with host acceptance matrix (superseded)

This records the intermediate host-matrix implementation. The direct-command
migration below removes that matrix and its preparation and launch adapters.

Source `584fca1` replaces the earlier preparation bridge and batch path with
ordinary `seedbox run`, `replay` and `inspect` calls. YAML selects the application,
Python driver, control/check service and limits. Guest Python owns the workload,
fault sequence and verification; host Python owns the acceptance matrix and
report. The diff against main `788fc43` contains only harness and documentation
changes. Existing Rust, guest supervisor, Python binding and Go/C workload
sources are unchanged.

The final bounded run uses four workers on `nixos.local`, CPUs 2–5 with SMT
siblings 18–21, in `/home/gvickers/seedbox-acceptance-drivers`. Its command is:

```sh
nix develop --command bash harness/acceptance.sh full \
  --root /home/gvickers/seedbox-acceptance-drivers \
  --kernel /home/gvickers/seedbox-scoped-faults-20261006/artifacts-shmem/bzImage \
  --cpus 2-5 \
  --report /home/gvickers/seedbox-acceptance-drivers/full-bounded-report.json
```

Evidence is retained under
`runs/acceptance-full-20261006T212656Z-2783040/` in that checkout. The live
acceptance, binding and runtime source files were compared with the local commit
by SHA-256. Runtime identities:

| Artifact | SHA-256 |
| --- | --- |
| CLI | `14e3d03c33cd2bcd6fe196314b573f8fef4e457094be6c512aab9a8d4ced0cb6` |
| VMM | `af0debdb6fe642543e771fe6e66efb56fecb7b29696d7508136b3cee37b082e3` |
| Kernel (current release-policy pin, K2) | `68bf3f024fc6ec0c01399020b5c37e4b42d661c2604f3c87fc879ccf9280e166` |

The historical run above used kernel `e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847`.
The Kernel row now names the K2 release-policy pin; these historical receipts
do not qualify that pair. Fresh AMD host run/replay acceptance is required.

Formatting, strict all-target Clippy, workspace tests and portable acceptance
passed on Darwin. Linux strict Clippy, portable acceptance and VMM tests passed.
All 23 acceptance unit tests passed, including error classification, bounded
overshoot retries, CPU lease waiting, cancellation cleanup, retained-output
integrity and required packet-drop evidence. Independent review passed.

Two earlier overlapping six-worker attempts were invalidated by host memory
exhaustion; they also encountered registry and temporary-image cleanup failures.
Both were cancelled cooperatively, their processes exited, every selected CPU
and SMT sibling matched its saved MSR value, and no restoration snapshots
remained. They are not acceptance passes. Both suite tiers now default to four
workers, and the final attempt runs alone.

The bounded full tier finished in 1119.4 seconds with **UNRESOLVED** (exit 3),
24 distinct executions and zero retries:

- Exit witnesses and full-state digests matched fresh replays.
- Undo 0/16 and margin 32768 preserved the reference witness.
- Four race seeds produced four different lost-update counts; the atomic control
  remained clean and seed 1 reproduced its sampled loss of 2,065,650 updates.
- The planted missing acknowledgement produced the expected FAIL.
- The short etcd workload passed after two stop/restart/partition/heal rounds,
  with eight confirmed effects and 20 partition-dropped packets.
- Ten of eleven etcd 3.5.3 seeds passed. Each completed all 20 fault rounds,
  recording 80 effects and 224–261 partition-dropped packets.
- Seed 4004 stopped with a VMM clock-bound error. It was neither retried nor
  counted as a workload failure or pass. This keeps the full tier UNRESOLVED.
- Cross-engine equivalence was skipped because this is not a performance-only
  engine change.

The unresolved execution is
`state/runs/a0835e9a061571fb-PSg7WD/` under the evidence directory. Its retained
`output/vmm.stderr` says:

```text
error: clock bound exceeded: time 3793645758 + 1000 exceeds hard stop 3793646723
```

Every used CPU and sibling matched the original MSR value `0x4480000000000`
after the suite, and no restoration snapshots remained. The suite report and
all staged projects, CLI commands, outputs and private run records are retained.
The clock-bound result remains an engine-validation blocker; no production
boundary rule or scenario seed was changed to make this run pass.

A separate diagnostic replay with identical retained inputs and VMM produced the
same clock-bound message at the exact same values. Its run is
`state/runs/a0835e9a061571fb-hj0gt0/`. The CLI correctly reports
`ERROR / NOT_COMPARABLE`: neither attempt produced a completed execution witness.
MSRs again matched the saved values, no snapshots remained, and no suite-owned
CLI or VMM processes were left running. This diagnostic does not replace the
unresolved suite result.

```sh
sudo -n target/debug/seedbox replay \
  /home/gvickers/seedbox-acceptance-drivers/runs/acceptance-full-20261006T212656Z-2783040/state/runs/a0835e9a061571fb-PSg7WD \
  --cpu 2 \
  --state /home/gvickers/seedbox-acceptance-drivers/runs/acceptance-full-20261006T212656Z-2783040/state \
  --vmm /home/gvickers/seedbox-acceptance-drivers/crates/seedbox-vmm/target/release/seedbox-vmm \
  --allow-engine-change --undo 256 --margin adaptive
```

At the time, an explicit `--vmm` required `--allow-engine-change`; in this
diagnostic the VMM hash is unchanged. #187 removed that flag: `--vmm` alone now
selects the engine.

## 2026-10-06 — direct CLI acceptance scenarios

Source `32bef7c` removes the host acceptance matrix, launcher, preparer and result
parsers. Each checked-in YAML file can be passed directly to `seedbox run`.
Compose configs supply the guest driver and Python API; build contexts reference
the existing Go/C source directories. Guest drivers own the packet-drop,
acknowledgement and atomic-control assertions. The planted negative control
expects checker exit 1 in YAML, making its normal CLI outcome PASS when the
checker detects the intended missing acknowledgement.

Source validation passed: six guest-driver tests on Darwin and Linux, four
public YAML validations, Darwin portable/workspace checks, formatting and strict
all-target Clippy. Independent review passed. Production Rust, the guest
supervisor, Python API and Go/C workload sources are unchanged.

Direct hardware validation uses the fresh checkout
`/home/gvickers/seedbox-direct-acceptance` on `nixos.local`, with CLI state under
`/home/gvickers/seedbox-direct-runs`. Commands use the CLI, VMM and kernel hashes
recorded in the preceding section. Runtime paths are supplied explicitly through
sudo, including the project shell's static BusyBox. There is no workload staging
or scenario-specific host runner. At most two commands run concurrently, on
CPUs 2 and 4 with SMT siblings 18 and 20.

The short etcd run (`--full-state`), race run, and sanity run all returned CLI
exit 0 / PASS. The sanity report records checker exit 1 as declared by its YAML.
The first long-run command, using the default adaptive margin, returned ERROR /
exit 2 with this VMM error:

```text
error: soft overshoot on n2: 12395 branches past (or an ambiguous overflow at) a soft stop before its syscall window opened
```

That failed execution remains at `runs/aa40e2f2e4b2ef56-dBjdxh` under the CLI state
directory. It is not a scenario pass. The follow-up command uses the same YAML
and seed with explicit `--margin 98304`; there is no automatic acceptance retry
or host verdict rewriting.

The short scenario replay at `--margin 32768` returned PASS / MATCH, including
all four full-state digests and the fault history. The first larger-margin long
command failed during image download with a registry TLS error, before guest
execution; it did not produce a run verdict. A subsequent invocation of the same
command is recorded separately.

Final direct-command results:

| Scenario / command option | CLI result | Retained run |
| --- | --- | --- |
| Short etcd, `--full-state` | PASS / exit 0 | `34679a6bcc7500bf-YlCWGH` |
| Race, default seed 1 | PASS / exit 0 | `b5b70ed0bfbdd580-jtlrVp` |
| Sanity, expected checker exit 1 | PASS / exit 0 | `11b4247ba69d5280-WehCHH` |
| Short replay, `--margin 32768` | PASS / MATCH / exit 0 | `34679a6bcc7500bf-jZCTdx` |
| Long etcd, default adaptive margin | ERROR / exit 2 (PMU overshoot) | `aa40e2f2e4b2ef56-dBjdxh` |
| Long etcd, `--margin 98304` | PASS / exit 0 | `ff080b00d539ec53-njFhCs` |

Run paths are relative to `/home/gvickers/seedbox-direct-runs/runs/`. The passing
long run completed twenty fault rounds, with 80 recorded effects and 245
partition-dropped packets. The guest driver checked the packet-drop increase and
acknowledgement ledger itself. Its freshly prepared client image differs from
the earlier failed attempt, so this is a separate passing scenario execution,
not proof of replay equivalence or an isolated diagnosis of the PMU failure.
The short replay is the retained-input performance comparison.

All nineteen relevant scenario, driver, binding and workload files were checked
by SHA-256 against source `32bef7c`. After all commands finished, CPUs 2/4 and SMT
siblings 18/20 exactly matched their saved MSR values (`0x4480000000000`). No
restoration snapshots or scenario-owned CLI/VMM processes remained. The earlier
clock-bound and current default-margin errors remain retained; this migration
does not claim they are fixed or that the full eleven-seed sweep is qualified.

The follow-up cleanup removes the superseded campaign runner/result parser and
their tests, five duplicate etcd/race world definitions, and `guest/race.init`.
Independent review found no remaining active callers. Darwin and Linux portable
checks passed after deletion, including the retained batch/MSR tests on Linux,
workspace tests, Linux VMM tests and all four YAML validations. Darwin formatting
and strict all-target Clippy passed. Scenario drivers and runtime source are
unchanged; hardware execution was not repeated for this removal-only cleanup.
The remaining raw kernel/NIC and one-VM comparison fixtures still need their
batch/MSR helpers and shared guest builders. Historical research remains available at commit `788fc43`.

## Local Mac TCG feasibility

The retained [experimental tool](../tools/mac-tcg/README.md) was measured on
Apple M4 Max / Mac16,6, macOS 27.0.1 / 26A434, QEMU 11.1.1 and stock Linux
6.18.54. The input-control probe had nine successful fresh processes over
three seeds, matching console and ordered observations within each seed;
median boot-to-exit time was 2.992 seconds. Omitted entropy, omitted icount,
planted failure and wall-time controls exposed their intended failures.
These are observation hashes, not an execution witness or full-state check.

The single-kernel etcd service fixture demonstrated namespace packet controls,
live directed partition/heal, process stop/restart, real recovery checks and
a planted missing-write failure. Five fresh seed-1009 fault runs matched
console, management logs and the historical scoped witness; median wall time
was 14.643 seconds. Their decoder records remain provisional. Two fresh runs
with the corrected decoder subsequently matched the original observations
and canonical witness; the rest of the historical matrix was not requalified.
The 39 service-phase attempts, including failures, used 573.504 QEMU seconds
under the original 600-second budget. This is feasibility evidence, not a
bug-discovery-rate comparison or qualification of a production Mac backend.

The corrected plugin hashes selected instruction dispatches and architectural
discontinuities up to explicit guest completion (`seedbox.tcg.boundaries.v2`).
It does not cover all instructions, registers, RAM or device state and is not
comparable to KVM witnesses. Shared-kernel clock/storage/failure limitations,
manual cached-input preparation and untested cross-host binary reproducibility
remain explicit. Exact executed sources, receipts, failed attempts and review
fixes are in the
[historical service report](https://github.com/gardnervickers/seedbox/blob/788fc4376b7e2cc96d1bc51ed2ae3d4635d16abd/research/mac-tcg/SERVICE_REPORT.md).

Historical single-VM networking measurements are likewise preserved in
[their report](https://github.com/gardnervickers/seedbox/blob/788fc4376b7e2cc96d1bc51ed2ae3d4635d16abd/research/single-vm-network/REPORT.md).
The 4.830-second versus 27.310-second medians (5.65x, 128 successful writes,
two repeats per layout) used the older ABI1 backend. They do not establish
the current PV/undo advantage or equal bug-finding coverage; #10 remains open.

## 2026-10-06 — final PR #27 merge validation

After merging main `8a3e14a`, formatting, strict all-target Clippy, workspace
tests, portable checks and all four YAML validations passed on Darwin and in
the fresh Linux checkout `/home/gvickers/seedbox-pr27-final-20261006`.
Linux VMM tests passed, including the updated fault-target lifecycle tests.
Independent final review passed. Main's removal of obsolete research and the
single-member etcd fixture is preserved.

The direct KVM results above retain their recorded engine identities; hardware
execution was not repeated for this merge. The seed-4004 clock-bound failure is
tracked in [issue #33](https://github.com/gardnervickers/seedbox/issues/33).

The runtime application paths in the historical evidence above have since moved
to `examples/runtimes/<runtime>/compose.yaml` (including TLS). They are optional
examples and no longer run in default acceptance; the recorded evidence is unchanged.
