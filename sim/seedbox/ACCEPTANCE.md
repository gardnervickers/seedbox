# Acceptance record

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

## Rust runner migration (2026-10-06, follow-up to #17)

The world-fixture suite, batch/campaign scheduler and Compose runtime assertions
now call the shared Rust orchestration/execution interfaces. Historical
failing-seed and witness pins were deleted; these results compare fresh runs or
check deliberately planted failures and application invariants.

Validation on Darwin and the qualified NixOS workstation:

- Portable suite passed, including Rust contract/scheduler/acceptance tests,
  shell syntax, MSR utility tests and Linux VMM unit tests. Final Rust formatting
  and Clippy checks (`--all-targets -- -D warnings`) passed on both hosts.
- Full fixture suite: all seven checks passed, including all 11 etcd 3.5.3
  seeds 4000–4010 after acknowledged stop/restart effects.
- Final smoke suite: exit/full-state replay, undo/margin invariance, race
  exploration (three observed outcomes), atomic control, planted checker
  failure, ordered stop/start recovery and explicit cross-engine replay passed.
- Ignored Rust Compose integration test: Redis and etcd PASS and planted FAIL
  runs replayed; full-state and changed-margin/undo replay, simulated timeout,
  wall expiry and CLI SIGTERM cancellation passed. Each execution verified
  exact MSR restoration on CPU 7 and sibling 23. An earlier CPU 8 attempt
  correctly refused an existing lease; the successful suite used a free core.
- Real batch seeds `1,1,2` retained three distinct attempts. A real campaign
  ran exactly `1009,1010,1011`, with zero unresolved attempts, and stopped at
  its three-seed cap.
- The updated CLI successfully inspected an existing version-2 run manifest
  retained by #17.

Evidence remains under `/home/gvickers/seedbox-rust-acceptance` on NixOS:
`acceptance-1/report.json` (full), `acceptance-final/report.json` (smoke and
cross-engine), `runtime-3/` (16 runtime attempts), `batch-1/`, and `campaign-1/`.
The full suite used the retained #17 VMM. The final smoke and runtime tests used
a new build of the same VMM source, with the old binary as the explicit
performance-only comparison. A final evidence-type relocation did not change
execution behavior or serialized fields; portable checks passed after that edit.

| Input / report | SHA-256 |
| --- | --- |
| Controlled kernel | `01f9c903a4754bfe9eca4c197cc3335ab8ff582944164982463518128639e0d8` |
| Retained #17 VMM | `a55ecd4a2d3b98f9a575e461695aebabcfe22a5e36d1572bbb3ce59a558289b8` |
| Rebuilt VMM | `c8a00e8624aa8a631acfd687631afe12764a424b197ae55f73b3fa0b3b177c06` |
| Full report | `e3387b9ad652c9a937b7702bc1a461cb29821a84f6e3d394e85de7eb3188af1f` |
| Final smoke report | `a66ec8194bec089861d3f7a5b9ed20c4f856d61fa5895132462125307df43ee8` |
| Batch report | `b7e790cb225ab5a48236eb7e9687981fdaf4da3b8ee92336dd321df304236ab4` |
| Campaign report | `b35190775914604d3ea53147337a595538f1db4523e4bb08bc2f2ed19b12d692` |

The final smoke suite's before/after readbacks matched exactly on CPUs 2–6 and
siblings 18–22 (`0x4480000000000`); the runtime readbacks matched on 7/23.
No restoration snapshots remained in either successful suite's attempt tree.
These report/input hashes identify this evidence; they are not acceptance pins.

The migration preserves low-level world definitions and existing guest archives.
It does not add Compose fault authoring, timestamp legacy acknowledgements, or
migrate historical research scripts recorded against older raw output formats.
