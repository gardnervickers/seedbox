# arm64 PV backends: first implementation milestone

This is an isolated experimental engine in `crates/seedbox-arm64`, based on
main `4a89dce`. It does not change the production x86 engine or its guest inputs.
The crate has its own workspace, lockfile and target directory. The only changes
to existing build files are the root workspace exclusion and the two aliases.

**Status: implementation and portable checks only.** Native HVF execution,
Linux unit-test execution, kernel compilation, nested KVM execution, timing and
cross-backend witness equality await the operator's hardware runs. No guest
Image hash or successful hardware witness is claimed yet. Do not treat a build,
a cross-target type check or a mock-machine test as hardware qualification.

Implementation-phase checks on the Mac: `cargo build-arm64` links
Hypervisor.framework; `cargo test-arm64` passes 12 tests with the HVF hardware
test ignored; crate clippy passes with warnings denied; three portable Python
input tests pass. The KVM adapter and all test targets cross-check for
`aarch64-unknown-linux-gnu` with an already-installed toolchain, without running
Linux binaries. Existing root formatting, clippy and portable acceptance
(including `cargo test --workspace --locked`) pass. The four frozen paths have
an empty diff against origin/main. No Lima VM or kernel tree was created here.

## Abstraction boundaries

| Boundary | Contract | Implementations / next step |
| --- | --- | --- |
| `backend::Backend` | Create VM, retain/map/unmap RAM, create one vCPU, read/write architectural core and system registers, run to an owned `Stop`, complete that operation, set an SPI line, query dirty pages. All calls and destruction happen on the creating thread. | `Hvf` links Hypervisor.framework; `Kvm` uses kvm-ioctls. Host handles, file descriptors, exit structures and register IDs stay inside these modules. |
| `machine::Machine<B>` | Own RAM, shared page, PL011, clockevent, PV clock and witness. Load Image/initrd/FDT; reconcile guest reads, perform device operations, judge timer expiry, inject while stopped, complete the exit, publish before re-entry. | Same loop for both backends. Backend selection exists only in CLI/tests. |
| `timebase::Timebase` | `prepare` publishes time and bound; `read` reconciles guest progress; `judge` determines expiry/idle advance. The trait uses numbers and `Publication`, not a hypervisor or machine type. | `PvClock` only. A future counted timebase needs a progress-sampling/stop-planning extension; it must not silently charge host elapsed time through this interface. |
| `devices::MmioDevice` | Width-checked register reads/writes produce values and explicit effects. Device effects never call a host API. | PL011, shared time-page register view, and clockevent registers. Machine owns IRQ line transitions. |
| `witness` | Canonical, versioned, little-endian exit encoding plus console and all guest RAM, including the shared page. | Host API raw structures, wall time and jitter are excluded. No RAM masks or skipped differing exits. |

`CoreReg::Sp` means SP_EL1, not the active stack selected by PSTATE. `SysReg`
uses the architectural 16-bit encoding. KVM translates its special core-register
locations and the historical swapped CNTVCT/CNTV_CVAL UAPI IDs locally.
The interface is backend-neutral but still **arm64-specific** in its register
and SPI vocabulary. Moving x86 underneath it later requires architecture-owned
register/interrupt descriptions and an x86 machine personality. Keep the
run/complete, mapping lifetime, device and timebase boundaries; do that migration
in a separately authorized engine-identity change, never by importing the frozen
x86 VMM here.

### Exit and completion semantics

`Exit` includes `Mmio { addr, len, write, data, reg }`, `Hvc { imm }`, `Wfi`,
`SysReg` and `Other(raw)`. `Stop.pc` always identifies the trapping instruction.
HVC APIs report the next PC; both adapters subtract four for the recorded stop
and do not advance it again on completion. HVF decodes ESR and applies load
sign/zero extension and advances MMIO PC itself. KVM owns MMIO completion on its
next `KVM_RUN`; the adapter writes the response into the held `kvm_run` buffer.
KVM doesn't expose MMIO's register number, so `reg` is optional and is not hashed.
Unknown exits, unexpected traps and missing capabilities fail the run.

Native arm64 WFI is consumed inside KVM and can block on host time. The patched
guest therefore writes the **same PV idle MMIO register on both backends**.
This is recorded as MMIO and given idle semantics by Machine. The ABI doesn't
pretend a hidden kernel WFI was a userspace exit. HVF also decodes native WFI;
it is not expected in this guest's executed idle path.

KVM requires the Linux 6.4+ SMCCC filter to forward the PSCI ranges
`0x84000000..0x84000020` and `0xc4000000..0xc4000020`. Machine provides PSCI 0.2,
MIGRATE_INFO_TYPE and SYSTEM_OFF, with unsupported functions returning -1.
The filter cannot forward the reserved architectural SMCCC range. This kernel
uses PSCI 0.2 and `mitigations=off`; expanding firmware discovery needs an explicit
shared policy. A missing filter is an error, not an in-kernel PSCI fallback.

HVF's macOS 15 `hv_gic` symbols are resolved with `dlsym` because the project
Nix shell's SDK is 14.4. The binary still links Hypervisor.framework. Setup checks
the SPI range and actual redistributor address against the FDT. It fails if any
required API is absent. HVF reports dirty tracking as `Unsupported`; KVM returns
its dirty bitmap and native host page size. Neither is used to omit RAM bytes
from the power-off hash.

## Guest layout and PV ABI 1

The FDT describes 128 MiB RAM, one CPU, GICv3, a PL011 with fixed 24 MHz input
clocks, PSCI/HVC, chosen/initrd/bootargs, and the PV node. It has no architectural
timer node. A domain-separated SHA-256 of the run seed supplies `rng-seed` and
`kaslr-seed`. KASLR is disabled for this first fixture, but the FDT retains the
seed property. NR_CPUS is 2 because that is Linux arm64's configuration minimum;
only CPU 0 exists in the FDT and `nosmp maxcpus=1` is supplied.

| Guest physical address | Purpose |
| --- | --- |
| `0x08000000`, `0x080a0000` | GIC distributor, first redistributor |
| `0x09000000` | PL011; SPI 33 (DT SPI number 1) |
| `0x0a000000`, 16 KiB | Shared time page, outside normal RAM |
| `0x0a010000`, 16 KiB | Clockevent MMIO; SPI 40 (DT SPI number 8) |
| `0x40000000`, 128 MiB | Zero-filled RAM; Image at its header's text_offset |
| `0x46000000` | initrd, at most 16 MiB |
| `0x47000000` | FDT |

The time page contains little-endian u64 `now`, `last`, `bound`, `abi` at offsets
0, 8, 16, 24. Host owns now/bound/abi; guest owns last. The kernel serializes reads
with local IRQ masking: return now if newer, otherwise last+1, store last. At
an exit Machine reconciles `max(now,last)` and republishes. Between exits,
computation costs zero time. Guest-local read progress becomes host-visible
only at the next exit, matching the shape of the x86 PV design.

Clockevent u64 writes: offset 0 arms an absolute deadline (zero cancels); 8
acknowledges and lowers the IRQ; 16 declares idle; 24 yields at the published
clock bound; 32 performs the ABI-1 handshake. Idle with an armed deadline jumps
to it and raises SPI 40 while stopped. Idle without a deadline or a pending
interrupt is an error. Crossing a polling bound yields, expires the timer and
republishes, so a clock poll cannot run past a deadline without an exit.
UART accesses and firmware calls have no extra time charge. PL011 transmission
is synchronous; its empty TX FIFO asserts a level IRQ when enabled, and guest
mask writes deassert it. Receive stays empty.

The reference semantics were read from
[PR #85](https://github.com/gardnervickers/seedbox/pull/85), head
`6fb165b265735f82196afb6e6aff791971b010ee`, including `timebase.rs`,
`machine/pv.rs` and `kernel/seedbox.c`. No code or dependency is shared with that
branch. This milestone omits syscall charging, counted stops, and fixed-point
spin detection. A guest computing forever without exits can still hang; use an
external operator timeout. A host timeout must fail, never advance virtual time.
The 5,000,000-exit limit catches exit-producing runaway guests.

## Kernel patch

`kernel-arm64/build.sh OUTPUT_DIRECTORY` downloads Linux **6.12.112**, verifying
archive SHA-256
`164dc9d1f6c93c61a15e1f071c48379b467f2b17c469cce7223471968208ed03`.
`apply.py` checks every replacement before writing anything and refuses reapply
or source drift. The patch:

- Removes arm64's unconditional ARM_ARCH_TIMER selection and installs the PV
  clocksource, sched_clock and one-shot clockevent in `time_init`.
- Routes generic and architectural kernel counter helpers, including get_cycles,
  through `seedbox_read`; pre-mapping reads use a deterministic early counter.
- Replaces counter-based WFxT/event-stream delay loops and native idle WFI with
  PV reads and MMIO idle. A polling read yields before crossing its clock bound.
- Clears CNTKCTL_EL1, including EL0VCTEN/EL0PCTEN and event-stream access; disables
  both architectural timer controls. EL0 CNTVCT/CNTPCT and their SS forms are
  handled through the kernel's synchronous trap path using PV time; CNTFRQ is
  the fixed 1 GHz virtual frequency.
- Makes vDSO time functions use syscalls, removing their counter instructions.
- Disables RNDR/RNDRRS and firmware entropy helpers. The seeded DT is trusted for
  initial entropy, while hardware RNG, EFI, PMU/perf, KVM, power management and
  optional CPU extension users are excluded by configuration.

`audit.py` disassembles vmlinux and the vDSO, refusing compiled MRS hardware
counter/frequency/RNG reads or WFIT/WFET instructions. It deliberately excludes
init's CNTVCT test instruction. This audit is a gate, not proof that every
future guest program or CPU feature is deterministic. HVF VTIMER_ACTIVATED is a
hard error; both backends also check CNTV_CTL at exposed exits. Do not mask a timer
leak to make a run pass.

The static musl `/init` prints MONOTONIC/REALTIME, verifies 10 ms and 1 s
nanosleeps, reads CNTVCT from EL0 twice, prints 32 getrandom bytes and an explicit
success marker, then calls reboot(POWER_OFF). Machine requires both that marker
and PSCI SYSTEM_OFF. A panic or early shutdown cannot print PASS.

## Operator commands

Run all commands from the checkout root unless explicitly inside the VM. The
operator owns Lima, kernel compilation, signing and hypervisor execution. Do
not run Lima/HVF/KVM from the workspace-write implementation sandbox.

### 1. Build guest inputs in the Ubuntu 24.04 arm64 VM

Use the operator's `vz`, nestedVirtualization-enabled VM with 4 CPUs and 8 GiB.
If creating one, put `LIMA_HOME` under a fresh short `/tmp/lima.XXXX` directory
so `ssh.sock` stays below the 104-character limit. Check `/dev/kvm` inside the VM.
Record `df -h` on Mac and VM before allocating/building. Do not modify the AMD host.

Inside the VM, with this checkout mounted at the same absolute path:

```sh
cd /Users/gardnervickers/.claude/worktrees/seedbox-astra-arm64
sudo apt-get update
sudo apt-get install -y build-essential musl-tools flex bison bc libssl-dev libelf-dev curl xz-utils python3
export SEEDBOX_ARM64_BUILD=/var/tmp/seedbox-arm64-build
export BUILD_JOBS=4
bash kernel-arm64/build.sh "$PWD/artifacts/arm64"
cat artifacts/arm64/SHA256SUMS
```

The output path may be on the Mac mount; all source extraction, object files and
static init compilation are under the VM's build directory. The script rejects
virtiofs/9p/NFS/FUSE build directories. Each invocation uses a fresh `run.XXXXXX`
subdirectory and preserves failed build evidence. Retain `Image`, `init.cpio`,
`kernel.config`, `SHA256SUMS`, `counter-audit.txt` and `build-directory.txt` until
acceptance is recorded. The archive hash above is **not** the built Image hash.

### 2. Build and codesign on the Mac

Inside the existing project Nix shell:

```sh
cargo test-arm64
cargo build-arm64
bash crates/seedbox-arm64/codesign.sh
cargo test-arm64 --test hardware --no-run --message-format=json > /tmp/seedbox-arm64-tests.jsonl
arm64_test_binary="$(python3 -c 'import json; print(next(x["executable"] for x in map(json.loads, open("/tmp/seedbox-arm64-tests.jsonl")) if x.get("reason") == "compiler-artifact" and x["target"]["name"] == "hardware" and x.get("executable")))')"
bash crates/seedbox-arm64/codesign.sh "$arm64_test_binary"
```

The script uses `codesign --force --sign -` with the shipped
`com.apple.security.hypervisor` entitlement, then verifies the signature.
Sign the test executable itself; signing just the CLI does not entitle tests.
Rebuilding either executable invalidates its entitlement signature. Run the
already-signed test executable directly after signing, avoiding a rebuild.

### 3. Native HVF test and CLI runs

```sh
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$PWD/artifacts/arm64/hvf-acceptance"
"$arm64_test_binary" --ignored --exact hvf_determinism --nocapture --test-threads=1

for case in 1a 1b 2; do
  seed="${case%[ab]}"
  crates/seedbox-arm64/target/release/seedbox-arm64 run \
    --kernel "$SEEDBOX_ARM64_KERNEL" --initrd "$SEEDBOX_ARM64_INITRD" \
    --seed "$seed" --backend hvf --witness "artifacts/arm64/hvf-cli-$case.json"
done
python3 crates/seedbox-arm64/compare.py artifacts/arm64/hvf-cli-1a.json artifacts/arm64/hvf-cli-1b.json
```

The ignored test runs seeds 1, 1, 2 and then 1 with a CPU burner and random
0–5 ms sleeps before vCPU runs. It saves four reports before comparing, verifies
repeatability and seed sensitivity, and requires the undisturbed HVF run to
finish in under one wall second including the guest's 1 s sleep and RAM hashing.
The guest independently checks actual sleep duration (within 20 ms). Jitter
runs can take minutes because the sleeps are deliberately per exit. Use a fresh
report directory for each attempt: evidence files are never overwritten.

### 4. Build and run KVM inside the VM

The root dev shell currently has no aarch64-linux output. Use the **same nixpkgs
revision from the project's lockfile** for an ephemeral arm64 Rust toolchain;
do not alter the root flake or install host packages. Nix must already be
available in the VM. From the mounted checkout, inside the VM:

```sh
arm64_nixpkgs_rev="$(python3 -c 'import json; print(json.load(open("flake.lock"))["nodes"]["nixpkgs"]["locked"]["rev"])')"
nix shell "github:NixOS/nixpkgs/$arm64_nixpkgs_rev#rustc" \
  "github:NixOS/nixpkgs/$arm64_nixpkgs_rev#cargo" \
  "github:NixOS/nixpkgs/$arm64_nixpkgs_rev#stdenv.cc" --command bash
```

Then, in that shell, use an explicit target to keep Linux and macOS build
artifacts separate. If `/dev/kvm` is not writable by the VM user, run with its
operator-provided KVM access rather than changing permissions on the Mac.

```sh
test -r /dev/kvm && test -w /dev/kvm
cargo test-arm64 --target aarch64-unknown-linux-gnu
cargo build-arm64 --target aarch64-unknown-linux-gnu
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$PWD/artifacts/arm64/kvm-acceptance"
cargo test-arm64 --target aarch64-unknown-linux-gnu --test hardware \
  -- --ignored --exact kvm_determinism --nocapture --test-threads=1
for case in 1a 1b 2; do
  seed="${case%[ab]}"
  crates/seedbox-arm64/target/aarch64-unknown-linux-gnu/release/seedbox-arm64 run \
    --kernel "$SEEDBOX_ARM64_KERNEL" --initrd "$SEEDBOX_ARM64_INITRD" \
    --seed "$seed" --backend kvm --witness "artifacts/arm64/kvm-cli-$case.json"
done
python3 crates/seedbox-arm64/compare.py artifacts/arm64/kvm-cli-1a.json artifacts/arm64/kvm-cli-1b.json
python3 crates/seedbox-arm64/compare.py artifacts/arm64/hvf-cli-1a.json artifacts/arm64/kvm-cli-1a.json
```

The KVM suite performs the same four determinism runs but does not impose the
native-HVF wall-time bound. Retain failures, including setup errors, rather than
switching to a host timer or retrying until witnesses happen to match.

## Witness interpretation and remaining gates

The SHA-256 domain begins `arch=arm64 timebase=pv`, followed by witness version 1.
Each exit encodes kind (u8), virtual ns/PC/address/data (four little-endian u64s),
length (u8), write (u8). MMIO reads record the supplied response. The final hash
binds trace count/hash, console length/hash and length-delimited RAM/page hash.
JSON also retains the full trace, exact console, seed, input hashes, exit count,
virtual ns and measured wall seconds. Host timing is report-only.

Cross-backend identity is a goal, **not yet demonstrated**. CPU ID/cache feature
registers and in-kernel GIC behavior can differ between HVF and nested KVM, even
on the same physical Mac. This implementation does not claim a normalized CPU
model. Such differences may appear in Linux's selected code paths, console,
register saves and RAM. Run `compare.py` and investigate the first divergence;
do not suppress CPU-dependent lines, skip exits or mask differing RAM. A trace
match with a RAM mismatch is still a mismatch. After input hashes match, retain
both JSON files and report the exact boundary needing further normalization.

Additional gaps: no syscall charging, no spin watchdog, no HVF dirty tracking,
no snapshots, no multi-vCPU or production application integration. Arbitrary
EL0 RNDR/PMU use beyond this controlled init fixture is not a qualified workload.
The guest patch, mandatory hv_gic API availability, architecture-timer removal,
vGICv3 and PSCI-forwarding support require operator verification. A failure at
any of those gates is a blocker to qualification; stop and return its exact error.

After successful operator runs, record per-backend seed-1/seed-2 witnesses,
repeat/jitter results, cross-backend equality or first divergence, Image/initrd
SHA-256s, full kernel config, compiler versions, macOS/VM kernel versions, exit
counts and run times. Draft PR creation is deferred by the operator's current
instruction; no push or PR is part of this implementation phase.

## Cleanup and portable checks

Before deleting the VM, copy retained artifacts and reports to the Mac and
verify `SHA256SUMS`. Remove the specific VM build directory created above only
after preserving any failure logs. Then delete the task's Lima VM, its temporary
LIMA_HOME and only newly created image-cache entries. Do not delete shared caches
or an existing VM. Record `df -h` before/after; this phase created no VM, image
cache entry or kernel build tree, so their cleanup belongs to the operator phase.

```sh
cargo fmt --manifest-path crates/seedbox-arm64/Cargo.toml --check
cargo clippy --manifest-path crates/seedbox-arm64/Cargo.toml --all-targets --locked -- -D warnings
cargo test-arm64
cargo build-arm64
python3 kernel-arm64/test_inputs.py
bash -n kernel-arm64/build.sh crates/seedbox-arm64/codesign.sh
git diff --stat origin/main -- crates/seedbox-vmm kernel guest crates/seedbox
```

Primary API references: [Linux KVM API](https://docs.kernel.org/virt/kvm/api.html),
[arm64 SMCCC filter](https://docs.kernel.org/virt/kvm/devices/vm.html#group-kvm-arm-vm-smccc-ctrl),
[Apple GIC functions](https://developer.apple.com/documentation/hypervisor/gic-functions),
and the installed SDK's `hv_gic*.h`/`hv_vcpu*.h` declarations.
