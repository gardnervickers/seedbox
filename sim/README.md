# Seedbox simulator

A deterministic world of real Linux machines on KVM, run by one host thread.
A seed plus the pinned inputs (kernel, initramfs, world config) regenerate the
same execution; seeds vary guest and simulator randomness, including timer and
interrupt timing.

## Layout

| Path | Contents |
| --- | --- |
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
  at deterministic points only: a natural VM exit, the guest's next syscall
  (a small kernel hook offers that exit), or an exact branch count reached by
  overflow interrupt plus hardware-breakpoint stepping (no TF). A timer
  interrupt fires at the first such point at or after its deadline, at most
  `stop_tolerance_ns` late. An overflow interrupt that skids past a stop, or
  past a syscall that would have been one, fails the run rather than moving
  the stop.
- **Guest kernel changes** (`kernel/`): kernel TSC reads return VMM-published
  virtual time; user RDTSC/RDTSCP trap via CR4.TSD and are emulated; a syscall
  hook offers yield exits.
- **Network and scheduling.** virtio-net over virtio-mmio into an in-memory
  fabric with seeded latency/jitter and directed partitions. The world runs the
  machine furthest behind in slices that never pass its causal horizon (the
  other machines' times plus link latency) or its next event. A slice may end
  at the first deterministic point in the last half-latency before that
  boundary (capped by `stop_tolerance_ns`), and events are delivered at the
  first stop up to that long before they are due: early by a bounded amount,
  never late, and never before they were sent.
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
sim/batch.sh sim/worlds/kill-3.5.2.json demo "7 7"         # two fresh runs of one seed
sim/campaign.sh sim/worlds/kill-3.5.2.json hunt 1000 8     # seeds until a failure, up to 8 h
```

Each batch run writes `runs/<name>-s<seed>-r<i>.json` (outcome, witness,
timing, per-machine stats), a `.status` file with its process exit status, and
per-machine console/management logs. Campaign batches use fresh directories
under `runs/<name>-batch.XXXXXX/`, preserving previous attempts. `batch.sh` pins
runs to consecutive cores from `CPU0` (default 2), runs the binary named by
`VMM`, and starts nothing unless the counter workaround is set and reads back.
`campaign.sh` accepts a client RESULT only with a zero process status and valid
terminal JSON for that seed. It logs incomplete attempts as `UNRESOLVED` and
exits nonzero. `SEEDBOX_ROOT` overrides the default `~/seedbox-sim` working
directory for either runner.

`SEEDBOX_MARGIN` sets how many branches before a stop the VMM switches from the
overflow interrupt to breakpoint stepping, and how early before a soft stop it
asks for syscall exits. It affects speed only. An overshoot (interrupt skid
larger than the margin) aborts the run; `campaign.sh` reruns it with a larger
margin for the identical result. Deterministic clock advances (timer-port I/O
charges and guest time reads) must also fit in the hard stop. Crossing it fails
with `clock bound exceeded`; no device I/O cost is clamped away, and a larger
PMU margin does not fix or retry this error.
