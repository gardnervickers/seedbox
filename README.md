# Seedbox

Run real Linux applications, inject failures, and replay the same execution.

![Inject and heal an etcd network partition in the Seedbox TUI](docs/assets/seedbox-tui.gif)

## Build

Requires Nix and a running Docker or Podman engine on Linux x86-64 or Apple
Silicon macOS. macOS also needs Apple's Command Line Tools.

```sh
git clone https://github.com/gardnervickers/seedbox.git
cd seedbox
seedbox_flake=$(bash tools/dev-flake.sh) && nix develop "$seedbox_flake"
cargo xtask build --release
alias seedbox="$PWD/target/release/seedbox"
```

Shell entry does not fetch DynamoRIO. The asset build initializes missing
submodules while preserving existing checkouts and local edits.

Inside `nix develop`, `bash harness/check.sh` runs the portable checks; `--help`
lists the native suites. On Linux x86-64, `bash harness/check-machine-tcg.sh`
prepares the native TCG fixtures and retained runtime, then runs portable and
native machine/public-service checks without KVM or an image engine.

## Run the etcd example

PV uses hardware virtualization (KVM on Linux, HVF on macOS); TCG emulates the
CPU in software and is slower but needs no hardware virtualization access.
Linux PV requires access to `/dev/kvm`.

```sh
seedbox server start

# PV
seedbox run examples/etcd --backend pv --paused --detach

# Or TCG
seedbox run examples/etcd --backend tcg --paused --detach
```

## Open the TUI

Use the simulation ID printed by `run`:

```sh
seedbox tui <sim-id>
```

Press **Space** to resume or pause, and **f** to inject or heal faults.

## `seedbox.yaml` format

`machines` defines the application nodes; `oracle` drives the workload and checks
the result. The oracle's exit code determines PASS (zero), INCOMPLETE (3, the
oracle could not decide) or FAIL (any other). Guest
[properties](docs/scenarios.md) can also fail a run, as can any process on any
machine dying of a crash signal or an out-of-memory kill, unless that machine
sets `allowCrashes: true`. Both use the same launch fields and require exactly
one of `image` or `build`.

For example, [the Redis project](examples/redis/seedbox.yaml) uses:

```yaml
name: redis-demo
machines:
  redis:
    image: docker.io/library/redis:7.2
    command: [redis-server, --appendonly, "yes", --save, ""]
oracle:
  build:
    context: ./workload
  environment:
    REDIS_ADDRESS: redis:6379
    SEEDBOX_PLANT_FAILURE: "0"
```

Machine names are hostnames on the simulated network. Build contexts are relative
to `seedbox.yaml`; `build.dockerfile` defaults to `Dockerfile`.

Optional launch fields are `entrypoint`, `command`, `environment`, `memoryMiB`,
`workingDirectory`, `user`, `files`, `volumes` and `allowCrashes`. `files` is a
list of `{source, target}` entries copied into the guest at preparation;
`volumes` maps names to `{target}` entries for writable disks that survive
machine crashes.
See [the etcd project](examples/etcd/seedbox.yaml) for a three-node cluster with
persistent data disks.

`network.latency` sets the one-way delay between machines and defaults to `1ms`.
`simulator.quantum` sets the timeslice:

```yaml
network:
  latency: 2ms
simulator:
  quantum: 500us
```

`quantum` defaults to `1ms` and must be a positive duration in whole nanoseconds.
Network latency, timers and scheduled events can shorten a parallel execution window.
Replay and checkpoints retain the quantum.

Choose the backend, seed, execution limits and machine threads with CLI flags.
`run` and `replay` accept `--machine-workers 1..64`; the default is one thread per
machine, with host CPUs shared across the server's execution slots. The count
affects host throughput without changing simulation results. Workers steal queued
machine runs, so machines can outnumber threads. Each round still waits for all
its running machines to reach the barrier.
