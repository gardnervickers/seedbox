# arm64 PV backends: first implementation milestone

This is an isolated experimental engine in `crates/seedbox-arm64`, based on
main `4a89dce`. It does not change the production x86 engine or its guest inputs.
The crate has its own workspace, lockfile and target directory. The only changes
to existing build files are the root workspace exclusion and the two aliases.

**Status: native HVF boots to init, but acceptance is blocked on a guest
kernel rebuild.** The 2026-10-07 native run exposed an EL0 counter-access leak
at exec. The shared machine now rejects it. No successful hardware witness,
noise invariance, or cross-backend equality is claimed. See the retained
failure and rebuild instructions below. Linux/KVM execution is still untested.

Current Mac checks: `cargo test-arm64` passes 14 portable tests; the hardware
test remains ignored by default. Crate formatting and clippy with warnings
denied pass, as do six Python input tests. The explicitly signed and executed
`hvf_determinism` test fails on the counter-access guard with the old Image.

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
The shared machine accepts zero writes to OSDLR_EL1 (`S2_0_C1_C3_4`,
`0x809c` = 32924) and OSLAR_EL1 (`S2_0_C1_C0_4`, `0x8084`). These preserve
the unlocked debug state; locking and other debug accesses remain errors.
The encodings follow the [ARM ARM debug-register tables](https://www.bitsavers.org/components/arm/ARM_Architecture_Reference_Manual_ARMv8_Rev_A.k_201609.pdf).
HVF completes a sysreg instruction by advancing PC by four and, for supported
reads, supplying the shared machine's response. Sysreg read witnesses include
that response. Unknown exits, unexpected traps and missing capabilities fail
the run.

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
- Clears CNTKCTL_EL1, including EL0VCTEN/EL0PCTEN and event-stream access; keeps
  EL0VCTEN cleared in `update_cntkctl_el1()` across exec, prctl and context
  switches, so PR_TSC_ENABLE permits PV emulation rather than hardware reads; disables
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
leak to make a run pass. After the PV handshake, Machine also requires
CNTKCTL_EL1 to remain zero at every exposed exit; a nonzero value fails without
rewriting it or resuming the guest. An instruction audit alone cannot detect
kernel writes which enable hardware counter reads by userspace.

The static musl `/init` prints MONOTONIC/REALTIME, verifies 10 ms and 1 s
nanosleeps, reads CNTVCT from EL0 twice, prints 32 getrandom bytes and an explicit
success marker, then calls reboot(POWER_OFF). Machine requires both that marker
and PSCI SYSTEM_OFF. A panic or early shutdown cannot print PASS.

## Operator commands

Run all commands from the checkout root unless explicitly inside the VM. The
operator owns Lima and guest kernel compilation. Native HVF execution and
signing from the workspace sandbox were explicitly authorized and worked in
the 2026-10-07 continuation. Do not rebuild the guest on the Mac.

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

## Native HVF failure and required rebuild (2026-10-07)

Host: macOS 27.0.1 (26A434), arm64; Nix rustc 1.98.1, cargo 1.98.0.
Retained inputs under ignored `artifacts/arm64/`:

- Image: `955fba78d17da9aa140690a3e29dc87dba9a41b36aa6cc9041562d60c59dcca7`.
- init.cpio: `9b952991625d7603e2db03e5ccdb87f78f1f7a16e6c664062505b7ddc75d8649`.
- Operator verified 72 Kconfig symbols and the vmlinux/vDSO instruction audit.

Handling the debug unlocks got two seed-1 diagnostic boots through init and
PSCI SYSTEM_OFF (2,179 completed exits each). Both failed init's CNTVCT check;
wall duration was not retained for those two boots. Their identical sleep
measurements were 10,000,124 ns and 1,000,050,007 ns. The second console was:

```text
Booting Linux on physical CPU 0x0000000000 [0x610f0000]
Linux version 6.12.112-seedbox-arm64-pv1 (seedbox@seedbox) (gcc (Ubuntu 13.3.0-6ubuntu2~24.04.1) 13.3.0, GNU ld (GNU Binutils for Ubuntu) 2.42) #1 SMP Thu Jan 1 00:00:00 UTC 1970
random: crng init done
Machine model: Seedbox arm64 PV ABI 1
earlycon: pl11 at MMIO 0x0000000009000000 (options '')
printk: legacy bootconsole [pl11] enabled
clock mono=485 real=486
clock mono=487 real=488
clock mono=489 real=490
sleep requested=10000000 elapsed=10000124
sleep requested=1000000000 elapsed=1000050007
cntvct first=4253969551655 second=4253969551655
SEEDBOX_INIT_FAIL: EL0 CNTVCT emulation: Result not representable
reboot: Power down
```

Live readback was `CNTKCTL_EL1=0x2`: EL0VCTEN enabled. Linux's
[`arch/arm64/kernel/process.c`](https://github.com/gregkh/linux/blob/v6.12.112/arch/arm64/kernel/process.c)
calls `do_set_tsc_mode(PR_TSC_ENABLE)` during exec; its
`update_cntkctl_el1()` helper re-enabled hardware reads after PV initialization.
The existing instruction audit correctly found no counter reads in the kernel
or vDSO, but that did not prevent init's deliberate counter instruction from
bypassing the kernel trap handler.

With the new guard, the CLI stops after 1,241 completed exits in 0.006664 s;
the signed `hvf_determinism` test stops on the same error after 1,241 completed
exits in 0.005754 s. These times end at failure, without final RAM hashing or a
1 s guest sleep. Logs: `artifacts/arm64/hvf-counter-guard.stderr` and
`artifacts/arm64/hvf-acceptance-counter-guard.log`. A separate launch after Cargo
had invalidated the CLI signature failed at VM creation with HV_DENIED (no
boot); signing again fixed that setup failure.

**Witnesses:** seed 1 first/repeat: none (failed); seed 2 and seed 1 with host
noise: not run after identifying the required rebuild. The integration suite
fails at its first boot. No getrandom output or PASS marker was reached. The
speed bound, repeat/noise comparisons and seed sensitivity remain unqualified.
VTIMER_ACTIVATED remains an error.

The fix is in `kernel-arm64/apply.py`: replace the enabling write in
`update_cntkctl_el1()` with a write clearing ARCH_TIMER_USR_VCT_ACCESS_EN.
The task's PR_TSC_SIGSEGV behavior is preserved. Its exact replacement anchor
was checked against pristine Linux 6.12.112 process.c and portable regression
tests pass; the revised Image has **not** been compiled or booted here.

The operator must rerun the normal builder **inside the Ubuntu arm64 VM**, from
this updated checkout, using a fresh output directory to retain the old input:

```sh
export SEEDBOX_ARM64_BUILD=/var/tmp/seedbox-arm64-build
BUILD_JOBS=4 bash kernel-arm64/build.sh "$PWD/artifacts/arm64-counter-fix"
```

Return the new Image, kernel.config, init.cpio, SHA256SUMS and counter-audit.txt.
Only the kernel source patch changed; the normal builder also regenerates the
unchanged init fixture. Resume native acceptance with those inputs and fresh
report paths. No artifacts were committed, and no changes were made to the
four frozen production paths.

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
