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
