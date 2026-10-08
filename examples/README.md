# Compose application examples

From the repository root:

```sh
nix develop
cargo build --release
# Start Docker/Podman for application preparation.
./target/release/seedbox run examples/etcd  # Mac
# Linux: add --timebase pv
./target/release/seedbox replay .seedbox/runs/ATTEMPT
```

These are the canonical application definitions. Run either `etcd/compose.yaml`
or `redis/compose.yaml` with the Rust `seedbox run` CLI, or create a self-contained
copy using `seedbox init DIRECTORY --template etcd` (or `redis`). Both paths use
these same Compose and workload sources.

The etcd example replaces the old `harness/worlds/cluster.json` and packages the
existing acknowledgement-ledger checker without changing its assertions. It runs
three etcd 3.5.2 members and eight writers with 20 acknowledged writes per writer;
the 10-second load duration is a deadline. The previous definition used timed
load, so these are functional acceptance cases, not matched throughput results.
The Redis example uses the official 7.2 image with AOF enabled and a Rust client
that checks PING and 32 SET/GET operations. Each workload implements readiness
retries and returns a nonzero exit for a failed assertion.

For intentional failure controls, set the Redis client's
`SEEDBOX_PLANT_FAILURE` to `"1"`, or append `--plant, missing` to the etcd client
command list. The runtime suite checks FAIL plus replay MATCH for both.

Each service gets a controlled Linux VM and private RAM-backed root filesystem;
application data does not persist across fresh runs. Use the release executable, which downloads and caches the patched kernel and
guest helpers on first use. Private releases require `GH_TOKEN` or `GITHUB_TOKEN`;
[manual Linux builds](../tools/runtime/README.md#build-without-private-release-access)
are also supported. Run on
Apple Silicon HVF or Linux KVM (`--timebase pv` avoids hardware-counter
requirements), with at least the tested 1024 MiB per service.
The image supplies userspace dependencies. Image versions, launch metadata and
filesystem layers are resolved and retained before execution.

See [build/run instructions](../README.md), [the supported subset](../crates/seedbox/README.md),
[acceptance evidence](../docs/acceptance.md), and [legacy fixture migration](../harness/worlds/README.md).
