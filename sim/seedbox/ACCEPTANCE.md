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
Those remain gates before issue #2 can be considered complete. The KVM suite is
prepared in `tests/accept-runtime.sh` and `tests/runtime-cases.sh`; its results are
not yet claimed here.
