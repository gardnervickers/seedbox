# Seedbox simulator

A deterministic world of real Linux machines on KVM, run by one host thread.
A seed plus the pinned inputs (kernel, initramfs, world config) regenerate the
same execution; seeds vary guest and simulator randomness, including timer and
interrupt timing.

## Layout

| Path | Contents |
| --- | --- |
| `seedbox/` | Rust configuration/orchestration library and validation/inspection CLI ([API and scope](seedbox/README.md)) |
| `vmm/` | Rust VMM and world scheduler (`seedbox-vmm`) |
| `kernel/` | Guest kernel patch, config and build script (Linux 6.12.112) |
| `guest/` | Initramfs builder, init scripts, the etcd workload client, test programs |
| `worlds/` | World configs: machines, network, scenario |
| `batch.sh`, `campaign.sh` | Run seeds in parallel, one pinned core per world |
| `msr54.py` | Sets/clears the AMD Zen counter workaround (see below) |

## How execution is controlled

- **Progress and time.** Guest progress is the host perf count of retired
  conditional branches (guest only, kernel and user). Virtual time advances
  1 ns per branch; idle machines jump to their next event.
- **Stops.** Events (timer deadlines, frames, scenario commands) are delivered
  at deterministic points only: a natural VM exit, the guest's next syscall at
  or after the event time (a small kernel hook offers that exit), or an exact
  branch count reached by overflow interrupt plus hardware-breakpoint stepping
  (no TF). The fallback stop is at most `stop_tolerance_ns` late.
- **Guest kernel changes** (`kernel/`): kernel TSC reads return VMM-published
  virtual time; user RDTSC/RDTSCP trap via CR4.TSD and are emulated; a syscall
  hook offers yield exits.
- **Network.** virtio-net over virtio-mmio into an in-memory fabric with
  seeded latency/jitter and directed partitions. Machines are scheduled by a
  conservative horizon (link latency) so no frame arrives in a machine's past.
- **Witness.** Each machine hashes every deterministic exit; `--digest` adds
  a hash of registers and all guest RAM; `--trace` writes every exit.

## Host requirements

- x86-64 AMD Zen host with KVM, run as root (guest-only perf counting).
- **The Zen SpeculativeLockMap workaround must be on** for the cores running
  simulations (MSR 0xC0011020 bit 54, rr's workaround). Without it, branch
  counts drift on lock-heavy code (e.g. Go) and runs diverge. `batch.sh` sets
  it on the pinned cores and their SMT siblings, then restores the originals.

## Build and run

On the workstation, inside `nix develop`:

```sh
./kernel/build.sh                                  # -> ../artifacts/bzImage
./guest/build-client.sh ../artifacts/client
./guest/build-initramfs.sh ../artifacts/cluster-3.5.2.cpio guest/cluster.init \
  ../src/etcd/v3.5.2/etcd:bin/etcd ../artifacts/client:bin/client
(cd vmm && cargo build --release)
```

From `~/seedbox-sim`:

```sh
./batch.sh sim/worlds/kill-3.5.2.json demo "1009 1009"   # two fresh runs of one seed
./campaign.sh sim/worlds/kill-3.5.2.json hunt 1000 8     # seeds until a failure, up to 8 h
```

Each run writes `runs/<name>-s<seed>-r<i>.json` (outcome, witness, timing,
per-machine stats) and per-machine console/management logs.

`SEEDBOX_MARGIN` sets how many branches before a stop the VMM switches from the
overflow interrupt to breakpoint stepping. It affects speed only. An overshoot
(interrupt skid larger than the margin) aborts the run; rerun with a larger
margin for the identical result.
