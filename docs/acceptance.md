# Acceptance record

Paths and commands below are as recorded, before the repository moved to a Cargo
workspace (`sim/seedbox` is now `crates/seedbox`, `sim/vmm` is `crates/seedbox-vmm`,
and the runners are under `harness/`). The engine hashes below are for builds
from that layout; a VMM built from `crates/seedbox-vmm` embeds different source
paths and is a new engine, even though its source is unchanged.

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
