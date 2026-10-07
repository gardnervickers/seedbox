# arm64 PV backends: first implementation milestone

This is an isolated experimental engine in `crates/seedbox-arm64`, based on
main `4a89dce`. It does not change the production x86 engine or its guest inputs.
The crate has its own workspace, lockfile and target directory. The only changes
to existing build files are the root workspace exclusion and the two aliases.

**Status: native HVF and operator-run nested KVM determinism acceptance PASS;
exit traces and console now MATCH across backends, but full RAM/witness equality
still FAILS.** The same idle-rendezvous Image from `180705e` boots reproducibly
on both. The shared CPU policy now hides Fine Grained Traps before boot,
removing the extra guest printk clock read responsible for HVF's 1 ns drift.
Both seeds' new HVF traces and consoles exactly match the operator's retained
`7e178d5` KVM reports. Full CPU/GIC normalization remains open; see the
[page diagnostics and policy](#power-off-page-diagnostics-and-architectural-policy-2026-10-07).

Current Mac checks: `cargo test-arm64` passes 19 portable tests; the two
hardware tests remain ignored by default. Crate formatting and clippy with
warnings denied pass, as do seven Python input tests and the root portable
acceptance checks. The explicitly signed `hvf_determinism` and `hvf_cpu_features`
tests pass. Earlier failures remain recorded below.

## Try it on your Mac

Requires an M-series Mac, macOS 15+, Xcode Command Line Tools, and Nix with
flakes enabled. This runs the experimental single-CPU PV fixture. The Linux VM
only builds its inputs; HVF executes the fixture directly on the Mac. Allow
about 24 GiB of VM disk and several minutes for the kernel build.

```sh
git clone --branch astra/arm64-hvf https://github.com/gardnervickers/seedbox.git
cd seedbox
nix develop
cargo build-arm64
bash crates/seedbox-arm64/codesign.sh

# Enter a temporary shell with Lima. Stay in the checkout root.
nix shell nixpkgs#lima -c bash
export LIMA_HOME="$(mktemp -d /tmp/sb-lima.XXXXXX)"
cat > "$LIMA_HOME/seedbox-build.yaml" <<'YAML'
vmType: vz
arch: aarch64
cpus: 4
memory: 8GiB
disk: 24GiB
images:
  - location: https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img
    arch: aarch64
mounts: []
containerd:
  system: false
  user: false
YAML
limactl start --name=seedbox-build --tty=false "$LIMA_HOME/seedbox-build.yaml"
limactl copy -r kernel-arm64 seedbox-build:/tmp/kernel-arm64
limactl shell --workdir /tmp seedbox-build sudo apt-get update
limactl shell --workdir /tmp seedbox-build sudo apt-get install -y \
  build-essential musl-tools flex bison bc libssl-dev libelf-dev curl xz-utils python3
limactl shell --workdir /tmp seedbox-build env BUILD_JOBS=4 \
  bash /tmp/kernel-arm64/build.sh /tmp/seedbox-arm64-inputs
mkdir -p artifacts/arm64-mac
limactl copy -r seedbox-build:/tmp/seedbox-arm64-inputs/. artifacts/arm64-mac/
(cd artifacts/arm64-mac && shasum -a 256 -c SHA256SUMS)
# Preserve the inputs on the Mac, then remove only this task's VM.
limactl stop seedbox-build
limactl delete --force seedbox-build
exit  # leave the Lima shell; return to nix develop in the checkout

for repeat in 1 2; do
  crates/seedbox-arm64/target/release/seedbox-arm64 run \
    --backend hvf --kernel artifacts/arm64-mac/Image \
    --initrd artifacts/arm64-mac/init.cpio --seed 1
done
```

Both runs print this stdout line; guest console and timing go to stderr:

```text
PASS: b3de4bbdaa7a747b54d3a940f02c4281bd3f26fe8574454c6cb7f06a3658a2a7
```

That is the current tested HVF witness with Image SHA-256
`7ea8fa2858ef33d05da559e9de5e5a9329c6d397e5b03fd7f4fcd3ebb75d9ff1`
and init.cpio SHA-256
`9b952991625d7603e2db03e5ccdb87f78f1f7a16e6c664062505b7ddc75d8649`.
These inputs were built with Ubuntu GCC 13.3.0 and binutils 2.42. Updated build
tools or a different Mac CPU feature profile can change the witness; repeated
runs with identical inputs on the same supported host must still match. Expected
fixture output includes `SEEDBOX_INIT_PASS`, 2,291 exits, and 1,010,051,041 virtual
ns. Sign again after any Cargo command that relinks the executable.

The configuration follows [Lima's VZ documentation](https://lima-vm.io/docs/config/vmtype/)
and uses Ubuntu's published arm64 cloud image. Nested virtualization is not
needed for this build-only VM. KVM execution is a separate operator step.

## Abstraction boundaries

| Boundary | Contract | Implementations / next step |
| --- | --- | --- |
| `backend::Backend` | Create VM, retain/map/unmap RAM, create one vCPU, read/write architectural core and system registers, run to an owned `Stop`, complete that operation, set an SPI line, query dirty pages. All calls and destruction happen on the creating thread. | `Hvf` links Hypervisor.framework; `Kvm` uses kvm-ioctls. Host handles, file descriptors, exit structures and register IDs stay inside these modules. |
| `machine::Machine<B>` | Own RAM, shared page, PL011, clockevent, PV clock and witness. Load Image/initrd/FDT; reconcile guest reads, perform device operations, judge timer expiry, inject while stopped, complete the exit, publish before re-entry. | Same loop for both backends. Backend selection exists only in CLI/tests. |
| `timebase::Timebase` | `prepare` publishes time and bound; `read` reconciles guest progress; `judge` determines expiry/idle advance. The trait uses numbers and `Publication`, not a hypervisor or machine type. | `PvClock` only. A future counted timebase needs a progress-sampling/stop-planning extension; it must not silently charge host elapsed time through this interface. |
| `devices::MmioDevice` | Width-checked register reads/writes produce values and explicit effects. Device effects never call a host API. | PL011, shared time-page register view, and clockevent registers. Machine owns IRQ line transitions. |
| `witness` | Canonical, versioned, little-endian semantic-exit encoding plus console and all guest RAM, including the shared page. | Host API raw structures, wall time, jitter and the two defined no-op debug completions are excluded. No RAM masks or comparison-time filtering. |

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
the unlocked debug state. When exposed, they are **backend-internal completions**,
not scheduling points: complete the instruction once without reconciling or
publishing PV time, judging timers, injecting IRQs, or recording an exit. The
raw-stop budget and timer/counter leak checks still apply. This rule is shared
by both backends and is limited to these exact zero writes; other exposed debug
operations (including reads, nonzero writes and other encodings) remain errors.
KVM handles debug register accesses internally, so this is not a claim that the
machine can reject all unexposed KVM debug use. This fixture does not use it.
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
a semantic exit Machine reconciles `max(now,last)` and republishes. Between exits,
computation costs zero time. Guest-local read progress becomes host-visible
only at the next exit, matching the shape of the x86 PV design.

Clockevent u64 writes: offset 0 arms an absolute deadline (zero cancels); 8
acknowledges and lowers the IRQ; 16 declares idle; 24 yields at the published
clock bound; 32 performs the ABI-1 handshake. Idle with an armed deadline jumps
to it and raises SPI 40 while stopped. New IRQ assertions require PSTATE.I to
be clear; masked admission fails without changing the guest mask. The updated
guest idle helper unmasks IRQs and executes ISB before the MMIO rendezvous, then
restores its incoming mask. The rebuilt guest passes the native HVF repeat/noise
acceptance below and the operator reports passing nested KVM acceptance; other
workloads still need hardware validation.
Idle without a deadline or a pending interrupt is an error. Crossing a polling bound yields, expires the timer and
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
The 5,000,000 raw-stop limit catches exit-producing runaway guests, including
repeated transparent debug writes. Report `exits` counts witnessed semantic stops.

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

These commands reproduce the now-verified idle-rendezvous Image. Preserve
`artifacts/arm64-irq-rendezvous/` and `artifacts/arm64-counter-fix/`, including
their evidence; use a fresh output directory for any future rebuild.

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
bash kernel-arm64/build.sh "$PWD/artifacts/arm64-irq-rendezvous"
cat artifacts/arm64-irq-rendezvous/SHA256SUMS
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
cargo test-arm64 --test hardware --no-run --message-format=json > /tmp/seedbox-arm64-tests.jsonl
arm64_test_binary="$(python3 -c 'import json; print(next(x["executable"] for x in map(json.loads, open("/tmp/seedbox-arm64-tests.jsonl")) if x.get("reason") == "compiler-artifact" and x["target"]["name"] == "hardware" and x.get("executable")))')"
bash crates/seedbox-arm64/codesign.sh
bash crates/seedbox-arm64/codesign.sh "$arm64_test_binary"
```

The script uses `codesign --force --sign -` with the shipped
`com.apple.security.hypervisor` entitlement, then verifies the signature.
Sign the test executable itself; signing just the CLI does not entitle tests.
Cargo can relink the CLI while building a test target, invalidating its
entitlement signature. Finish all Cargo builds before signing both executables.
Run the already-signed test executable directly, avoiding a rebuild.

### 3. Native HVF test and CLI runs

```sh
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-irq-rendezvous/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-irq-rendezvous/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$PWD/artifacts/arm64-irq-rendezvous/hvf-acceptance"
"$arm64_test_binary" --ignored --exact hvf_determinism --nocapture --test-threads=1

for case in 1a 1b 2; do
  seed="${case%[ab]}"
  crates/seedbox-arm64/target/release/seedbox-arm64 run \
    --kernel "$SEEDBOX_ARM64_KERNEL" --initrd "$SEEDBOX_ARM64_INITRD" \
    --seed "$seed" --backend hvf --witness "artifacts/arm64-irq-rendezvous/hvf-cli-$case.json"
done
python3 crates/seedbox-arm64/compare.py artifacts/arm64-irq-rendezvous/hvf-cli-1a.json artifacts/arm64-irq-rendezvous/hvf-cli-1b.json
```

The ignored test runs seeds 1, 1, 2 and then 1 with a CPU burner and random
0–5 ms sleeps before vCPU runs. It saves four reports before comparing, verifies
repeatability and seed sensitivity, and requires the undisturbed HVF run to
finish in under one wall second including the guest's 1 s sleep and RAM hashing.
The guest independently checks actual sleep duration (within 20 ms). Jitter
runs can take minutes because the sleeps are deliberately per exit. Use a fresh
report directory for each attempt: evidence files are never overwritten.

### 4. Build and run KVM inside the VM

Historical FGT-only procedure. For the current commit use
[Next KVM page capture](#next-kvm-page-capture), which includes the new policy.

Use the VM's existing `$HOME/.cargo/bin/cargo`; **Nix is not installed there**.
The explicit Linux target keeps Linux and macOS build artifacts separate. Run
from this mounted checkout after the operator has made the new commit available.
No kernel/initrd rebuild is needed. If `/dev/kvm` is not writable, use the
operator-provided KVM access rather than changing permissions on the Mac.

```sh
set -euo pipefail
cd /Users/gardnervickers/.claude/worktrees/seedbox-astra-arm64
export PATH="$HOME/.cargo/bin:$PATH"
test -r /dev/kvm && test -w /dev/kvm
(cd artifacts/arm64-irq-rendezvous && sha256sum -c SHA256SUMS)
"$HOME/.cargo/bin/cargo" test-arm64 --target aarch64-unknown-linux-gnu
"$HOME/.cargo/bin/cargo" build-arm64 --target aarch64-unknown-linux-gnu
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-irq-rendezvous/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-irq-rendezvous/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$PWD/artifacts/arm64-irq-rendezvous/kvm-fgt-policy"
test ! -e "$SEEDBOX_ARM64_REPORT_DIR"
mkdir "$SEEDBOX_ARM64_REPORT_DIR"
"$HOME/.cargo/bin/cargo" test-arm64 --target aarch64-unknown-linux-gnu --test hardware \
  -- --ignored --exact kvm_cpu_features --nocapture --test-threads=1 \
  > "$SEEDBOX_ARM64_REPORT_DIR/cpu-features.log" 2>&1
cat "$SEEDBOX_ARM64_REPORT_DIR/cpu-features.log"
"$HOME/.cargo/bin/cargo" test-arm64 --target aarch64-unknown-linux-gnu --test hardware \
  -- --ignored --exact kvm_determinism --nocapture --test-threads=1 \
  > "$SEEDBOX_ARM64_REPORT_DIR/determinism.log" 2>&1
cat "$SEEDBOX_ARM64_REPORT_DIR/determinism.log"
for case in 1a 1b 2; do
  seed="${case%[ab]}"
  crates/seedbox-arm64/target/aarch64-unknown-linux-gnu/release/seedbox-arm64 run \
    --kernel "$SEEDBOX_ARM64_KERNEL" --initrd "$SEEDBOX_ARM64_INITRD" \
    --seed "$seed" --backend kvm --witness "$SEEDBOX_ARM64_REPORT_DIR/cli-$case.json"
done
python3 crates/seedbox-arm64/compare.py "$SEEDBOX_ARM64_REPORT_DIR/cli-1a.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-1b.json"
python3 crates/seedbox-arm64/compare.py "$SEEDBOX_ARM64_REPORT_DIR/kvm-seed1-a.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-1a.json"
python3 crates/seedbox-arm64/compare.py "$SEEDBOX_ARM64_REPORT_DIR/kvm-seed1-a.json" "$SEEDBOX_ARM64_REPORT_DIR/kvm-seed1-jitter.json"
python3 crates/seedbox-arm64/compare.py "$SEEDBOX_ARM64_REPORT_DIR/kvm-seed2.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-2.json"
```

Then retain the **full** cross-backend comparisons. They are expected to report
all recorded exits matching and equal console hashes, but exit 1 for RAM/witness
mismatch. The explicit exit check preserves that failing gate; it does not turn
trace-only equality into full acceptance.

```sh
for case in 1a 2; do
  rc=0
  python3 crates/seedbox-arm64/compare.py \
    "artifacts/arm64-irq-rendezvous/hvf-fgt-final/cli-$case.json" \
    "$SEEDBOX_ARM64_REPORT_DIR/cli-$case.json" \
    > "$SEEDBOX_ARM64_REPORT_DIR/cross-$case.log" 2>&1 || rc=$?
  cat "$SEEDBOX_ARM64_REPORT_DIR/cross-$case.log"
  test "$rc" -eq 1
done
```

Expected KVM witnesses remain seed 1 `ed30d3bb…` and seed 2 `f024317d…` if its
FGT field is already absent. The inventory records its actual MMFR0 (`0xc038`)
before/after policy plus other feature/cache registers, which the old reports
lack. HVF's MMFR0 changes `0x010010000f100023` to `0x000010000f100023`.
The new KVM result is still operator-pending; the matching comparisons so far
use the retained `48e25a1` reports. The KVM suite has the same four boots but
no native-HVF wall-time bound. Preserve failures; never substitute a host timer
or retry until a witness happens to match.

## Witness interpretation and remaining gates

The SHA-256 domain begins `arch=arm64 timebase=pv`, followed by witness version 1.
Each semantic exit encodes kind (u8), virtual ns/PC/address/data (four little-endian u64s),
length (u8), write (u8). MMIO reads record the supplied response. The final hash
binds trace count/hash, console length/hash and length-delimited RAM/page hash.
JSON also retains the full trace, exact console, seed, input hashes, exit count,
virtual ns and measured wall seconds. Host timing is report-only.

Cross-backend identity is a goal, **not yet demonstrated**. CPU ID/cache feature
registers and in-kernel GIC behavior can differ between HVF and nested KVM, even
on the same physical Mac. This implementation does not claim a normalized CPU
model. Such differences may appear in Linux's selected code paths, console,
register saves and RAM. Run `compare.py` and investigate the first divergence;
do not suppress CPU-dependent lines, filter differing semantic exits or mask
differing RAM. The two exact no-op debug writes are classified before machine
scheduling, never removed by comparison tooling. A trace
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

## Historical counter-access failure and rebuild (2026-10-07)

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

## Counter-fixed HVF acceptance and IRQ rebuild requirement (2026-10-07)

**Full HVF acceptance: FAIL.** Host: macOS 27.0.1 (26A434), arm64;
Nix rustc 1.98.1 and cargo 1.98.0. The operator built the guest from `131b88f`
with GCC 13.3.0 / GNU ld 2.42 inside the Ubuntu arm64 VM. All three entries in
`artifacts/arm64-counter-fix/SHA256SUMS` were independently verified here:

- Image: `689e875fdff5abbfd0db9a897d767408d81fc906640f565259ca7a030f4ee194`.
- init.cpio: `9b952991625d7603e2db03e5ccdb87f78f1f7a16e6c664062505b7ddc75d8649`.
- kernel.config: `facb82e4dbad040a19a553304413975b72e6c8eeb8b511414729a7c21a71a822`.

The full config and passing vmlinux/vDSO counter audit remain beside the Image.
No guest rebuild or VM management was performed on the Mac.

The signed CLI reached `SEEDBOX_INIT_PASS` and PSCI SYSTEM_OFF in 0.278166 s,
with 2,293 exits and witness
`d404557f65c5c40cd844b91bd1d2fbbd95d80b0a9abaaa76bdb4c1d548eaac13`.
The independently signed hardware test then produced these four reports under
`artifacts/arm64-counter-fix/hvf-acceptance/` (wall duration includes final RAM
hashing; the noise boot includes the deliberate sleeps and CPU burner):

| Boot | Witness | Wall seconds | Exits |
| --- | --- | ---: | ---: |
| Seed 1 first | `d404557f65c5c40cd844b91bd1d2fbbd95d80b0a9abaaa76bdb4c1d548eaac13` | 0.248102917 | 2,293 |
| Seed 1 repeat | `d404557f65c5c40cd844b91bd1d2fbbd95d80b0a9abaaa76bdb4c1d548eaac13` | 0.249114625 | 2,293 |
| Seed 1 with host noise | `34c3a6aee8a625e1fbe43ef7bc1373e6ed79a1a34c52727616c4f7c0569de8da` | 8.502266333 | 2,293 |
| Seed 2 | `26432352c9f2cc0905f75cd8da2665be8773ea55a8302ba2d2a2cf7c8e2c8165` | 0.245261667 | 2,293 |

All four boots powered off after init success; virtual time was 1,010,051,041 ns.
Both sleeps passed init's independent check: 10,000,124 ns for 10 ms and
1,000,050,007 ns for 1 s. The undisturbed full boot was below one wall second.
The seed-1 console, identical under noise, was:

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
cntvct first=1010051036 second=1010051037
getrandom d2626d690cce59bd6c7efc9146cc2973429249c4a64ba13b251b1913aef10e6f
SEEDBOX_INIT_PASS
reboot: Power down
```

The signed test exited 101: the noise witness differed. Trace hash and console
hash matched exactly, but RAM hashes were
`b84ee58a81d38fb7726a7616571fca5a848314951987d39f4332896f49c8ba48`
and `54a1785003901ba8c64fa3e8a57b8b6f5c4e4804a2a0cc6639c7f1613ed9eb30`.
The complete log is `artifacts/arm64-counter-fix/hvf-acceptance.log`.

### Diagnosis and retained failed experiments

Temporary RAM captures found six differing 64-bit words between two additional
seed-1 boots: the initial kernel stack at GPAs `0x404b3c00`/`0x404b3c08` and the
four-word interrupt entropy pool at `0x47fd7ab0..0x47fd7acf`. The stack retained
a `ct_irq_enter` return address. The adjacent timer callback points to
`mix_interrupt_randomness`, identifying the pool independently of its bytes.
`ram-difference.json` retains those exact words and surrounding pool state.
Symbols were decoded from this Image's kallsyms and relocated using its runtime
base; no different kernel's symbol map was substituted.

Temporary ELR_EL1 readback at timer acknowledgements identified the cause. In a
noisy boot, the IRQ at virtual time 890,000,413 ns interrupted at
`0xffff800080319b84`, immediately after `DAIFClr`, rather than the usual
`0xffff80008005d964`, after returning from the idle helper. Linux 6.12.112's
`add_interrupt_randomness()` mixes this interrupted PC into its four-word pool.
The exposed MMIO trace therefore matched while the stack and RAM did not.
The readback log is `hvf-irq-diagnostic.log` beside the inputs.

For completeness, the diagnostic boots below all had 2,293 exits. RAM capture
and register logging add overhead, so these durations are not performance
qualification. `D` denotes the full seed-1 witness above and `S2` the full seed-2
witness above.

| Evidence path relative to the input directory | Witness | Wall seconds |
| --- | --- | ---: |
| `hvf-diagnostic/hvf-seed1-a.json` | `aa74435a0bffdf9e8aa5b044404c15e8d98c75e3583e1db36fab3f2d53fb33ed` | 0.371258417 |
| `hvf-diagnostic/hvf-seed1-b.json` | D | 0.383035750 |
| `hvf-irq-diagnostic/hvf-seed1-a.json` | D | 0.359943291 |
| `hvf-irq-diagnostic/hvf-seed1-b.json` | D | 0.354223125 |
| `hvf-irq-diagnostic/hvf-seed2.json` | S2 | 0.381134042 |
| `hvf-irq-diagnostic/hvf-seed1-jitter.json` | `5429da8e0e96b5fb8d94d5798fece393963f7eb8d9ddcad0ca7919887185ec3d` | 7.421001625 |
| `hvf-irq-diagnostic-0.log` | D | 0.331993 |
| `hvf-irq-diagnostic-1.log` | D | 0.349387 |

The first diagnostic suite failed on its second ordinary seed-1 boot, establishing
that deliberate noise is not necessary for the drift. A subsequent diagnostic
suite again failed on its noise boot. No retry was treated as qualification.

A temporary shared-machine single-step experiment deferred IRQ assertion until
unmasking, but stepping disrupted an exclusive-access loop. It failed at the
5,000,000-exit limit in 9.122925 s; its log is `hvf-step-diagnostic.log`. All
stepping changes and diagnostic instrumentation were removed. A separate CLI
launch after Cargo relinked the executable failed at VM creation with HV_DENIED
(`hvf-cli-1b.stderr`); it produced no boot or witness. Signing again resolved
that setup error.

### Changes requiring the next guest rebuild

- `kernel-arm64/seedbox.c`: `seedbox_idle()` now saves the incoming IRQ mask,
  enables IRQs, executes ISB, writes the PV idle register, and restores the
  original mask. Thus the shared machine can assert the timer at an already
  unmasked exit instead of queuing it across the later guest DAIFClr.
- `crates/seedbox-arm64/src/machine.rs`: every new timer/UART SPI assertion
  requires PSTATE.I clear. Masked admission fails without rewriting PSTATE or
  invoking the backend injection API. Deassertions remain allowed while masked.
- Regression tests exercise rejection of masked admission and compile the actual
  guest idle helper against an IRQ/MMIO model, checking unmasked/synchronized
  MMIO and preservation of both incoming mask states.

This is a candidate fix awaiting a rebuilt guest, **not** a demonstrated
interrupt-placement solution. If drift persists with unmasked admission, exact
IRQ-entry control needs further work. No witness bytes, guest RAM, entropy,
trace entries, host-noise settings, or comparison checks were removed or masked.
VTIMER_ACTIVATED and counter leaks remain hard errors.

With the final guard and the supplied counter-fixed Image, the signed CLI exits
2 after 1,400 completed exits in 0.006535 s; the signed `hvf_determinism` test
exits 101 after 1,400 completed exits in 0.006161 s. Both fail at the first idle:

```text
SPI 40 asserted with PSTATE.I masked: nondeterministic interrupt admission; rebuild kernel-arm64 with the unmasked PV idle rendezvous
```

Logs: `hvf-irq-guard.log` and `hvf-acceptance-irq-guard.log` beside the inputs.
No final-code successful witnesses exist yet. The earlier table records the
actual pre-guard observations, not qualification of the committed code.

Stop here for the operator's guest rebuild inside the Ubuntu arm64 VM, from
this updated checkout, into a fresh directory:

```sh
export SEEDBOX_ARM64_BUILD=/var/tmp/seedbox-arm64-build
BUILD_JOBS=4 bash kernel-arm64/build.sh "$PWD/artifacts/arm64-irq-rendezvous"
```

Return Image, init.cpio, kernel.config, SHA256SUMS and counter-audit.txt. Init and
config sources are unchanged; the builder regenerates and audits the complete
input set. Resume signed native acceptance with those inputs and new report
paths. KVM/cross-backend acceptance remains untested. No frozen production path
was modified, and no push or PR was performed.

## Idle-rendezvous HVF acceptance (2026-10-07)

**Full native HVF acceptance: PASS.** Tested engine and guest source: `180705e`,
on macOS 27.0.1 (26A434), arm64, with Nix rustc 1.98.1 / cargo 1.98.0.
The operator rebuilt the guest with GCC 13.3.0 / GNU ld 2.42 in the Ubuntu arm64
VM. All three entries in `artifacts/arm64-irq-rendezvous/SHA256SUMS` were verified:

- Image: `7ea8fa2858ef33d05da559e9de5e5a9329c6d397e5b03fd7f4fcd3ebb75d9ff1`.
- init.cpio: `9b952991625d7603e2db03e5ccdb87f78f1f7a16e6c664062505b7ddc75d8649`.
- kernel.config: `facb82e4dbad040a19a553304413975b72e6c8eeb8b511414729a7c21a71a822`.

The complete config and passing vmlinux/vDSO audit remain beside the inputs.
The independently signed `hvf_determinism` executable passed on its first suite
with this Image. Reports are in `hvf-acceptance/` beside the inputs and the full
log is `hvf-acceptance.log`. Durations include final RAM hashing; host noise
includes the unchanged CPU burner and random 0–5 ms sleeps before vCPU runs.

| Boot | Witness | Wall seconds | Exits |
| --- | --- | ---: | ---: |
| Seed 1 first | `f1fdc70f77465ad30c6d8e1e7a93b7fe56cbb1da2362299ec16f296154b8b44a` | 0.246172625 | 2,293 |
| Seed 1 repeat | `f1fdc70f77465ad30c6d8e1e7a93b7fe56cbb1da2362299ec16f296154b8b44a` | 0.244623917 | 2,293 |
| Seed 1 with host noise | `f1fdc70f77465ad30c6d8e1e7a93b7fe56cbb1da2362299ec16f296154b8b44a` | 7.641374917 | 2,293 |
| Seed 2 | `88aaedb029adaee65886a1cb0dc71a99d8881b2515d944631a0ea998586cb03b` | 0.274207250 | 2,293 |

All four boots reached init PASS and PSCI SYSTEM_OFF at virtual time
1,010,051,041 ns. `compare.py` independently returned MATCH for first/repeat,
first/noise and first/CLI. All seed-1 runs have trace hash
`333b9566af89bdf98484b0a093eadcb8c5a3dcfeef52110df6c9506c4b1600a3`, console hash
`d3d4fd4c99f72a30c636039b23b756acaa2a21bc9df1b2c55b8630820690b570`, and RAM hash
`4ded95524ffbd8e5616b91ca8b3219653883956f5611e452e195c97e01418b77`.
The signed CLI took 0.255688167 wall seconds and 2,293 exits;
its witness is the same as seed 1 above. Its complete output is retained in
`hvf-cli-signed-1a.log`, with its report in `hvf-cli-signed-1a.json`:

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
cntvct first=1010051036 second=1010051037
getrandom d2626d690cce59bd6c7efc9146cc2973429249c4a64ba13b251b1913aef10e6f
SEEDBOX_INIT_PASS
reboot: Power down
```

Thus the one-second nanosleep advanced guest MONOTONIC by 1,000,050,007 ns
while the entire undisturbed boot completed in about a quarter wall second.
The 10 ms sleep advanced it by 10,000,124 ns. Seed 2 also passed both sleep
checks and produced a different witness.

### First divergence and root cause

Re-running the existing compare tooling on the original
`arm64-counter-fix/hvf-acceptance/hvf-seed1-a.json` and
`hvf-seed1-jitter.json` reports **all recorded exits match**: there is no first
diverging exit record among the 2,293 entries. Their console hashes also match.
The first differing witness component is RAM, with hashes `b84ee58a…` and
`54a17850…`; the complete hashes remain in the historical record above.
The exit trace does not record asynchronous IRQ entry or ELR_EL1, so matching
exposed MMIO exits did not imply matching guest execution between exits.

The retained IRQ diagnostic run supplies the first differing interrupted PC:
at virtual time 890,000,413 ns, ELR_EL1 is `0xffff800080319b84` under noise
versus `0xffff80008005d964` without noise. This diagnostic pair is distinct from
the original `d404557f…` / `34c3a6ae…` pair; the original reports alone cannot
identify its exact first internal divergence. The saved RAM comparison locates
drift in the initial kernel stack and four-word interrupt entropy pool.
The timer was being asserted while DAIF.I was masked, so delivery after the
subsequent guest unmask could interrupt different instructions. Linux mixes
that interrupted PC into interrupt randomness, preserving host-dependent IRQ
placement in guest-visible RAM even when console and exposed exits agree.

The fix was already in `180705e`: `seedbox_idle()` unmasks IRQs and executes
ISB before the idle MMIO, then restores the caller's mask. The machine admits
new SPI assertions only at an unmasked exit. This continuation used the
operator's rebuilt Image to validate that fix; no further engine or kernel
change or guest rebuild was needed. No witness fields, RAM bytes, entropy,
trace records or noise checks were removed. Counter leaks and
VTIMER_ACTIVATED remain hard errors.

### Checks and remaining scope

`cargo test-arm64` passes all 15 portable tests, and the explicitly signed
hardware suite passes. arm64 fmt and clippy (`--all-targets --locked -- -D warnings`)
pass, as do seven Python input tests and shell syntax checks. Root seedbox fmt,
clippy, workspace tests and `bash harness/acceptance.sh portable` also pass;
the latter log is retained as `portable-acceptance.log`. Linux-only
`cargo test-vmm` was not run on this Mac.

One preliminary CLI invocation failed before boot with HV_DENIED after Cargo's
test build invalidated the CLI signature (`hvf-cli-1a.log`). Signing the CLI
again resolved it; the operator sequence above now builds all targets before
signing. This failed launch produced no guest witness and was not a determinism
retry. No HVF acceptance gate remains failing. KVM, cross-backend equality and
broader workloads remain untested. Frozen production paths are unchanged.
This continuation changes only this acceptance document; artifacts remain
ignored, and no push or PR is performed.

## Transparent debug completion and cross-backend analysis (2026-10-07)

### KVM evidence supplied by the operator

The operator ran engine `451ddd4` in the Ubuntu arm64 Lima VM (`vz`, nested KVM,
nVHE), using the exact Image/initrd hashes recorded above. `cargo test-arm64`
passed, and the ignored `kvm_determinism` test passed all four boots:

| Boot | Witness | Exits | Wall seconds |
| --- | --- | ---: | ---: |
| Seed 1 first/repeat | `ed30d3bb6b1c4d6418cbd33beffa3be4699e1732a2ea6a26e4a8d3dd1c87b073` | 2,291 | about 0.49 |
| Seed 1 with host noise | `ed30d3bb6b1c4d6418cbd33beffa3be4699e1732a2ea6a26e4a8d3dd1c87b073` | 2,291 | 6.75 |
| Seed 2 | `f024317d83bff80afbcbf712c21cacf96c2ab18c0d10765593f1f538eb1b57cf` | 2,291 | not supplied |

Virtual time was 1,010,051,041 ns. CLI runs also passed with the same witnesses.
`kvm-cli-1a.json` and `kvm-cli-2.json` are retained beside the inputs and were
inspected on the Mac; the repeat/noise results above are operator-reported.
The seed-1 CLI report records 0.507982151 wall seconds.

### Two debug exits, followed by a separate progress mismatch

The original `hvf-cli-signed-1a.json` versus `kvm-cli-1a.json` comparison first
differs at zero-based index 1203. HVF exposes OSDLR_EL1=0 at
PC `0xffff800080013d08`, time 307 ns, followed by OSLAR_EL1=0 at
`0xffff800080013d0c`, also 307 ns. KVM exposes neither. Its index 1203 is the
PL011 peripheral-ID read at address `0x09000fe0`, PC `0xffff80008020282c`, time
331 ns; HVF's corresponding read was index 1205, time 332 ns.

Linux's debug initialization clears both locks. The
[Linux 6.12 KVM handlers](https://github.com/torvalds/linux/blob/v6.12/arch/arm64/kvm/sys_regs.c)
consume OSDLR as read-as-zero/write-ignore and update OSLAR's lock bit in-kernel.
With the fixture's unlocked state, both zero writes have no guest-visible effect.
[KVM_SET_GUEST_DEBUG](https://docs.kernel.org/virt/kvm/api.html#kvm-set-guest-debug)
configures single-step and breakpoint/watchpoint events; it is not an API for
forwarding these MSR accesses as matching userspace SysReg exits. Adding debug
execution machinery would not define a suitable common scheduling boundary.

The implemented policy completes just these two no-op write forms before
clock reconciliation and timer/IRQ decisions. HVF still advances their PC by
four through its normal `complete` method; KVM needs no change. Publication
occurs at startup and after semantic completions, never on an internal stop.
All other exit handling, leak checks, IRQ-mask guards, RAM hashing and host-noise
checks remain intact. The 5,000,000 raw-stop budget includes internal stops;
`Report.exits` and trace count include only semantic stops. The witness encoding
and its format version remain 1; the change is in which events are scheduling
points, not the byte encoding. Old and new HVF witnesses intentionally differ.

**The original attribution to a per-exit time charge was incorrect.** `PvClock`
charges no fixed cost for exits: it reconciles `max(now,last)` and advances to
deadlines. Guest PV reads increment `last`. At the two debug stops, the first
deadline (10,000,011 ns) is still far away. Republishing `now=last` does not
change the next guest read (`last+1`). The new boots experimentally confirm that
removing these scheduling points changes only the trace/count; HVF's console
and complete RAM hashes are exactly the same as before.

Comparing the **unfiltered new HVF trace** with the retained KVM report now finds:

- Both have 2,291 records. Every kind, PC, address, width and direction matches.
- First difference: index 1203, the PL011 ID read above, **332 vs 331 ns**, data
  17 on both. The previous common exit (1202) arms 10,000,011 ns at time 11 ns,
  PC `0xffff80008001df30`. Thus the new difference is guest progress between
  those two semantic boundaries, not a userspace exit charge or MMIO decoding.
- 495 record timestamps differ by exactly +1 ns on HVF: indices 1203–1397 and
  1494–1793. All other timestamps agree, including final virtual time.
- 101 timer-arm values at `0x0a010000` are +1 on HVF. The first is index 1404:
  time 10,001,023 ns on both, deadline 20,000,413 vs 20,000,412 ns.
- The only other data difference is UART output at index 1486 (`0x34` vs `0x35`),
  console character 514: the 10 ms sleep reports 10,000,124 vs 10,000,125 ns.
  The rest of the console, including getrandom bytes, matches.
- RAM still differs: HVF `4ded95524ffbd8e5616b91ca8b3219653883956f5611e452e195c97e01418b77`,
  KVM `325b0ad204e5e8b60f55afb8bac1f07d36a8ee72f372a4365817c6952420472d`.

The reports do not contain instruction traces, CPU feature readbacks or RAM
bytes, so they cannot identify the extra guest PV read's call site or locate
all differing RAM. Both adapters use their host's default vCPU model; KVM uses
`KVM_ARM_PREFERRED_TARGET`, HVF uses a null vCPU configuration. CPU feature/cache
registers and in-kernel GIC behavior remain possible causes, not proven diagnoses.
The next deeper investigation needs matched guest-progress/architectural-state
diagnostics from both hosts around exits 1202–1203. Do not subtract 1 ns, alter
timer arms, filter console bytes or mask RAM to force equality.

### New native HVF acceptance

The same verified Image/initrd were used without a guest rebuild. All Cargo
builds finished before signing the CLI and test executable. The signed hardware
suite passed on its first attempt, retaining reports under
`artifacts/arm64-irq-rendezvous/hvf-transparent-debug/` and its log in
`hvf-transparent-debug.log` beside that directory:

| Boot | Witness | Wall seconds | Semantic exits |
| --- | --- | ---: | ---: |
| Seed 1 first | `f2394e95b08172d3eb398748f36a35710a2b58044a56a05d88768671ae9b10a5` | 0.237980625 | 2,291 |
| Seed 1 repeat | `f2394e95b08172d3eb398748f36a35710a2b58044a56a05d88768671ae9b10a5` | 0.238356625 | 2,291 |
| Seed 1 with host noise | `f2394e95b08172d3eb398748f36a35710a2b58044a56a05d88768671ae9b10a5` | 8.380519084 | 2,291 |
| Seed 2 | `cd779f002d6d106d99410d578d7b1af15dd1f065754e8bb32aaec2e1a7a10f07` | 0.238297458 | 2,291 |

All boots passed init and PSCI SYSTEM_OFF at 1,010,051,041 ns. Signed CLI
seed 1 twice and seed 2 match the corresponding test witnesses; reports/logs
are `cli-1a`, `cli-1b`, and `cli-2` in that directory. Seed-1 trace hash is
`6b3ee22a33d09e560b228b110b215face0b2249d00a6e713ffb9bd48b6e8ff8f`;
console and RAM hashes are unchanged from the previous HVF acceptance.

Regression coverage compares a mock backend exposing the unlocks against one
consuming them internally, with guest progress reaching an armed deadline
before the writes. It checks identical full witnesses, no premature publication
or IRQ admission, and exactly-once completion. Negative controls retain rejection
of nonzero writes, reads, unknown debug registers, timer/counter leaks and masked
IRQ admission. `cargo test-arm64`, arm64 fmt/clippy, seven Python input tests,
root fmt/clippy/workspace tests and portable acceptance pass. The portable log
is `portable-transparent-debug.log`. Linux-only `cargo test-vmm` cannot run here.
Frozen production paths are unchanged; no guest rebuild, push or PR is needed.

### Expected next VM result

Historical expectation at `48e25a1`, superseded by the FGT continuation below.

Run section 4 on the updated commit, keeping the same inputs and using the new
`kvm-transparent-debug` report directory. No guest rebuild is required. Because
KVM exposes neither debug write, its semantic execution is unchanged; its new
seed-1 witness is expected to remain `ed30d3bb…`, not become `f2394e95…`.
The hardware suite should still pass. The final cross-backend comparison is
expected to exit 1 at the PL011 read described above. Retain that result and
any new divergence; equality remains an open gate. Native HVF and nested KVM
repeatability are separate demonstrated properties.

## FGT feature policy and the extra PV read (2026-10-07)

### Root cause of the 1 ns shift

The operator's `48e25a1` reports are now present in
`artifacts/arm64-irq-rendezvous/kvm-td/`: seed 1, host-noise seed 1 and CLI
witnesses are `ed30d3bb…`; seed 2 is `f024317d…`. This confirms that making
debug unlocks transparent did not change KVM execution. Both backends have
2,291 semantic exits and finish at 1,010,051,041 ns.

The time difference comes from **guest CPU feature discovery**, not a counter
emulation charge. Before the fix, native HVF exposes
`ID_AA64MMFR0_EL1=0x010010000f100023`, including FGT=1 in bits 59:56.
The stopped guest RAM at exit 1203 contains an extra boot-ring message:
`CPU features: detected: Fine Grained Traps` at RAM offset `0x559018`.
This message is suppressed from the PL011 console by `loglevel=4`, but it is
still formatted and timestamped in the printk ring.

Linux's FGT entry in `arm64_features` matches that field; system feature setup
calls `update_cpu_capabilities(SCOPE_SYSTEM)`, which logs a newly detected
capability. See [Linux cpufeature.c](https://raw.githubusercontent.com/torvalds/linux/v6.12/arch/arm64/kernel/cpufeature.c).
`vprintk_store()` samples `local_clock()` before console filtering; that reaches
`sched_clock()` and the registered `seedbox_read()`. See
[Linux printk.c](https://raw.githubusercontent.com/torvalds/linux/v6.12/kernel/printk/printk.c).
This is the additional PV page read. The feature bit also remains in the
kernel's saved feature/capability state, explaining why fixing timestamps
alone would be wrong.

A controlled HVF experiment changed only MMFR0.FGT before guest execution:

- Exit 1202 still has page `(now,last,bound,abi) = (10,11,U64_MAX,1)` before
  handling the timer-arm write. Machine then publishes `(11,11,10000011,1)`.
- At exit 1203, before reconciliation, page `last` is 332 with FGT exposed and
  331 with FGT absent. No deadline is due anywhere in this interval.
- Extracting the stopped RAM's boot-ring text shows exactly one message
  removed, the FGT detection above. The following RCpc detection message now
  occupies offset `0x559018`. No log-level or printk code was changed.
- With FGT absent, **every exit record and every console byte matches retained
  KVM for both seeds**, including timer-arm values and the 10 ms sleep result.
  The seed-1 first PL011 ID read is now 331 ns, and the 10 ms sleep prints
  10,000,125 ns. Final time and exit count are unchanged.

Temporary RAM/register dump instrumentation was removed before acceptance.
Its evidence is retained as `diagnostic-baseline.log`, `diagnostic-no-fgt.log`,
`diagnostic-no-fgt.json`, `diagnostic-{1202,1203}.ram`,
`diagnostic-no-fgt-{1202,1203}.ram`, and the two `*-1203-printk.txt` files beside
the inputs. The text extraction includes leftover bytes after record lengths;
it is a diagnostic view, not a replacement for witness data. The unmodified
baseline diagnostic still produces `f2394e95…`; disabling FGT produces
`b3de4bbd…`. This isolates the feature setting, not host timing or a guest rebuild.

The code paths exclude the other proposed charging mechanisms. Both adapters
only decode/complete exits; neither owns a PV clock. Machine has no accepted
counter/frequency SysReg read path: exposed reads fail. EL0 CNTVCT traps into
the patched guest kernel's PV emulation. The sole accepted exposed sysregs are
the two zero debug writes, completed without publication. Initial X registers,
SP_EL1, PC, PSTATE=0x3c5, SCTLR, CNTKCTL=0 and CNTV_CTL=0 are already initialized
by shared code. Initial publication and all later reconciliation use the same
Machine implementation. No charge has been added, removed or moved there.

The old KVM JSON contains no feature inventory, so its absent FGT field is an
inference from the controlled experiment, not a claimed live KVM register
readback. The new `kvm_cpu_features` test in section 4 will retain that readback.

### Shared policy and validation

`Machine::new` now calls `configure_cpu_features` immediately after vCPU
creation, before any guest instruction. This EL1-only machine clears the whole
FGT field and verifies the **entire** MMFR0 readback, preserving every other
field. HVF changes to `0x000010000f100023`; an already-zero KVM field requires
no write. Unsupported access, rejected writes or mismatching readback fail
setup. There is no silent fallback or per-backend clock adjustment.

Portable regression coverage exercises all FGT field values, preservation of
every other nibble, rejection of silently ignored/rejected writes, and an
already-absent feature on a read-only backend. The mock boot checks the feature
policy is in place before its first run. The ignored `hvf_cpu_features` and
`kvm_cpu_features` tests print host-default and configured CPU feature registers
without booting a guest. The signed native inventory is retained in
`hvf-cpu-features.log`; inaccessible cache registers are explicitly reported
as unavailable, not fabricated as zero.

Native HVF acceptance passed with the final code and the same verified inputs.
Reports are under `hvf-fgt-final/`, and the suite log is `hvf-fgt-final.log`:

| Boot | Witness | Wall seconds | Exits |
| --- | --- | ---: | ---: |
| Seed 1 first | `b3de4bbdaa7a747b54d3a940f02c4281bd3f26fe8574454c6cb7f06a3658a2a7` | 0.248581958 | 2,291 |
| Seed 1 repeat | `b3de4bbdaa7a747b54d3a940f02c4281bd3f26fe8574454c6cb7f06a3658a2a7` | 0.252555000 | 2,291 |
| Seed 1 with host noise | `b3de4bbdaa7a747b54d3a940f02c4281bd3f26fe8574454c6cb7f06a3658a2a7` | 7.305678041 | 2,291 |
| Seed 2 | `3477c8e474cbe111646f5a6aa03acadffefc5bb2995212bcd2aafec648e6dabe` | 0.249710083 | 2,291 |

All reach init PASS and PSCI SYSTEM_OFF at 1,010,051,041 ns. CLI seeds 1, 1
and 2 match the corresponding test witnesses. The first suite, before adding
the inventory test, also passed; its evidence remains in `hvf-fgt-policy/`.
Neither suite was a retry of a determinism failure.

### Remaining RAM mismatch and expected KVM result

The precise remaining witness differences are:

| Component | New HVF | Retained KVM at `48e25a1` |
| --- | --- | --- |
| Seed-1 trace | `e91d56cedf3c759bebba911a8300f89e4b1b80dfd337e996f37e6c0d8879994a` | Same |
| Seed-1 console | `ce862ae5fa2d8bd06489a02fafb04fabea1cef32a57a76f2ab57f80f57139323` | Same |
| Seed-1 RAM | `b74b96c6af79d93ab84fb34d3737944ed44393a1229ff285a273687b4cd98dc0` | `325b0ad204e5e8b60f55afb8bac1f07d36a8ee72f372a4365817c6952420472d` |
| Seed-2 trace | `3df66d85acd202aa54e129cd7c46cc8f6978d5653ecb7ea0a51fcef8a1c5e5fa` | Same |
| Seed-2 console | `944b94bd80a944953989004b1b7b72292bccb3086f9e18c4d90517da4c1f5c38` | Same |
| Seed-2 RAM | `8bebd1e539e8a02a5f4315320009aeb15bd842d7347cd4a1860de21596481844` | `43b929b1342fa7e74264e258332ed660ca06e7715d0f6b7b74e206e7a466ffdd` |

Thus `compare.py` still correctly exits 1. The FGT policy is a principled fix
for the identified extra read, **not a complete common architectural profile**.
The remaining RAM byte locations and causes cannot be determined from KVM's
hash-only report. Both adapters still inherit other CPU/cache feature defaults
and use distinct in-hypervisor GIC implementations. Native HVF's retained boot
ring, for example, advertises 988 SPIs and RSS. The KVM adapter never configures
`KVM_DEV_ARM_VGIC_GRP_NR_IRQS`. These are concrete remaining configuration gaps,
not proof of which bytes account for this RAM hash mismatch. Closing this gate
requires matching KVM initial-register and stopped-RAM captures, including the
printk ring, then an explicit common CPU/GIC model. Blindly clearing more feature
bits or changing an IRQ model without that evidence is not justified.

Section 4 uses the VM's existing Cargo and retains a new CPU inventory plus
determinism reports. Expected KVM witnesses are unchanged (`ed30d3bb…` for seed
1, `f024317d…` for seed 2), with exactly matching HVF traces and console and a
remaining RAM mismatch. This expectation is pending the operator's run of the
new commit; no new KVM execution is claimed here. No guest rebuild is needed.

arm64 fmt/clippy and all 16 portable Rust tests pass; seven Python input tests,
shell syntax, root seedbox fmt/clippy/workspace tests, and portable acceptance
also pass (`portable-fgt-policy.log`). Linux-only `cargo test-vmm` was not run
on the Mac. No witness normalization, RAM masks, trace filtering, clock charge
adjustments, relaxed leak checks or changed noise gates were introduced.
Frozen production paths are unchanged; no push or PR is performed.

## Power-off page diagnostics and architectural policy (2026-10-07)

The operator reports `cargo test-arm64`, `kvm_cpu_features` and KVM repeat/noise
acceptance passing at `7e178d5`. The supplied `kvm-fgt-policy/` reports confirm
all 2,291 exits and console bytes match HVF for both seeds, while RAM differs.
Seed-1 KVM witness is `ed30d3bb…`, seed 2 `f024317d…`. The CPU inventory log was
not in the copied directory, so other KVM register values are not inferred from
those hash-only reports.

### Diagnostic format and timing

`run --ram-pages FILE` reserves a new output file before creating the VM and
writes JSON only after successful PSCI SYSTEM_OFF, acceptance-marker validation,
and final PV publication. It hashes every 4 KiB guest page, including zero pages:
32,768 main RAM pages plus four pages covering the separate 16 KiB PV mapping.
This uses the kernel's 4 KiB granularity rather than HVF's host mapping alignment.
Region `base` and `length` are byte addresses/sizes; `sha256[i]` covers offset
`i * page_size`. The sidecar includes the witness, RAM hash, seed and exact input
hashes to identify its paired report. It contains hashes, not RAM contents.

The diagnostic is disabled by default, is not a Report field, and does not
change witness encoding, clock reconciliation, memory, or the measured run
wall time. There is no extra vCPU entry. An existing output path is refused;
a failed run can leave an empty reserved file, and an output I/O failure is an
error rather than PASS. `compare-pages.py LEFT RIGHT` prints differing page
indices, offsets and GPAs; exits are 0 equal, 1 different, 2 invalid/incompatible.
Continue using `compare.py` for full witness acceptance.

### What is normalized, and what remains host-dependent

Linux stores CPU feature registers in boot CPU data and sanitized feature tables.
It also records GIC discovery and CPU/cache details in the suppressed printk
ring, with temporary values surviving in stacks or freed allocations. Thus
identical console and semantic exits need not imply equal RAM. These remain
likely causes until KVM page hashes locate the changed pages; no RAM bytes are
masked or excluded.

| Input | Policy and evidence |
| --- | --- |
| MMFR0.FGT | Existing shared policy clears bits 59:56, preserving all other fields with exact full-register readback. |
| MPIDR | Shared policy requires `0x80000000` (one CPU, affinity zero); skip an already-equal write, then require exact readback. HVF already had this value; KVM no longer implicitly owns topology policy. |
| PMU | Shared policy clears DFR0.PMUVer bits 11:8 and checks the entire register, retaining all debug fields. HVF reads `0x10305006`. KVM never enables `KVM_ARM_VCPU_PMU_V3`. No PMU device/IRQ is advertised. |
| GIC interrupt count | Shared profile has 1024 IDs, including private/reserved IDs: 988 usable SPIs. KVM now sets `KVM_DEV_ARM_VGIC_GRP_NR_IRQS=1024` before initialization and reads back exactly 1024. HVF has no count setter; its `GICD_TYPER.ITLinesNumber` must equal 31 or setup fails. Live HVF TYPER is `0x0478001f`. This normalizes count only, not the whole TYPER. |
| MIDR / REVIDR | HVF MIDR is `0x610f0000`; REVIDR API reads fail with `HV_BAD_ARGUMENT`. Linux KVM's invariant-register API checks MIDR/REVIDR against host values rather than allowing an arbitrary replacement. No invented common model is installed. |
| Other ID_AA64 fields | Preserved and inventoried. There is no retained KVM inventory here to justify additional feature changes. Defining a complete profile requires matching inventories and support constraints; blindly clearing capability nibbles is unsafe. |
| CTR_EL0 / DCZID / CLIDR / CNTFRQ | All four HVF sysreg API reads return `HV_BAD_ARGUMENT` on this Mac. Exact readback through this adapter is unavailable, so no normalization is claimed. These can still be read by guest instructions and may survive in RAM. Architectural counter/timer leak rejection stays strict. |
| GIC IIDR / remaining TYPER fields | HVF IIDR API read returns `HV_BAD_ARGUMENT`; PIDR2 reads `0x30`. KVM ignores writes to read-only distributor registers except explicitly documented fields. RSS, ID width, priority capabilities and implementation identity remain an open common-GIC-model problem. |
| PSCI | Already common: shared HVC handling reports version 0.2, MIGRATE_INFO_TYPE=2 and unsupported services=-1. KVM forwards these calls via its SMCCC filter. No host PSCI version enters this interface. |

See [KVM VGIC API](https://docs.kernel.org/virt/kvm/devices/arm-vgic-v3.html),
[Linux 6.12 register implementation](https://raw.githubusercontent.com/torvalds/linux/v6.12/arch/arm64/kvm/sys_regs.c),
and [Apple GIC API](https://developer.apple.com/documentation/hypervisor/gic-functions).
The native inventory is retained in `hvf-ram-pages/cpu-features.log`. The new
KVM interrupt count intentionally changes an architectural input and can change
its RAM/witness. Its new witness must be measured, not assumed to remain the old
`ed30d3bb…`. Complete cross-backend equality remains open.

### Native acceptance and checks

Final native evidence is under `artifacts/arm64-irq-rendezvous/hvf-ram-pages/`.
The same verified Image/initrd were used; no guest rebuild was performed.

| Boot | Witness | Wall seconds |
| --- | --- | ---: |
| Seed 1 first | `b3de4bbdaa7a747b54d3a940f02c4281bd3f26fe8574454c6cb7f06a3658a2a7` | 0.259302 |
| Seed 1 repeat | Same | 0.251053 |
| Seed 1 host noise | Same | 7.419816 |
| Seed 2 | `3477c8e474cbe111646f5a6aa03acadffefc5bb2995212bcd2aafec648e6dabe` | 0.248365 |

All have 2,291 exits and finish at 1,010,051,041 ns. CLI runs with page hashing
for seeds 1, 1 and 2 match the corresponding ordinary hardware-test reports.
The seed-1 page sidecars have zero differing pages. HVF witnesses and RAM hashes
are unchanged from `7e178d5`, as these policies already match its defaults.

19 portable ARM64 Rust tests, ARM64 fmt/clippy, seven Python guest-input tests,
shell syntax, root fmt/clippy/workspace tests and portable acceptance pass.
Tests cover diagnostic witness neutrality, all pages including zeros, refusal to
overwrite output, and ignored/rejected CPU-policy writes. Linux KVM execution still needs
the operator's hardware run. The project Nix Rust toolchain lacks Linux std;
subsequent all-target Linux `cargo check` and clippy with warnings denied passed
using the already installed nightly cross-target sysroot and a separate `/tmp`
target directory (`linux-check.log`, `linux-clippy.log`). Linux-only `cargo test-vmm` was not run. Frozen production paths are unchanged.
The clean-checkout Lima recipe was checked against the CLI documentation but
not executed here; the operator owns new guest builds.

### Next KVM page capture

Run in the existing nested KVM VM on this commit; no guest rebuild is needed.
Keep the CPU inventory and all sidecars alongside the reports. This supersedes
section 4's historical FGT-only output directory and expected KVM witnesses.

```sh
set -euo pipefail
cd /Users/gardnervickers/.claude/worktrees/seedbox-astra-arm64
export PATH="$HOME/.cargo/bin:$PATH"
(cd artifacts/arm64-irq-rendezvous && sha256sum -c SHA256SUMS)
cargo test-arm64 --target aarch64-unknown-linux-gnu
cargo clippy --manifest-path crates/seedbox-arm64/Cargo.toml \
  --target aarch64-unknown-linux-gnu --all-targets --locked -- -D warnings
cargo build-arm64 --target aarch64-unknown-linux-gnu
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-irq-rendezvous/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-irq-rendezvous/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$PWD/artifacts/arm64-irq-rendezvous/kvm-ram-pages"
mkdir "$SEEDBOX_ARM64_REPORT_DIR" # fresh directory; preserve previous evidence
cargo test-arm64 --target aarch64-unknown-linux-gnu --test hardware \
  -- --ignored --exact kvm_cpu_features --nocapture --test-threads=1 \
  > "$SEEDBOX_ARM64_REPORT_DIR/cpu-features.log" 2>&1
cargo test-arm64 --target aarch64-unknown-linux-gnu --test hardware \
  -- --ignored --exact kvm_determinism --nocapture --test-threads=1 \
  > "$SEEDBOX_ARM64_REPORT_DIR/determinism.log" 2>&1
for case in 1a 1b 2; do
  seed="${case%[ab]}"
  crates/seedbox-arm64/target/aarch64-unknown-linux-gnu/release/seedbox-arm64 run \
    --backend kvm --kernel "$SEEDBOX_ARM64_KERNEL" --initrd "$SEEDBOX_ARM64_INITRD" \
    --seed "$seed" --witness "$SEEDBOX_ARM64_REPORT_DIR/cli-$case.json" \
    --ram-pages "$SEEDBOX_ARM64_REPORT_DIR/cli-$case.pages.json" \
    > "$SEEDBOX_ARM64_REPORT_DIR/cli-$case.log" 2>&1
done
python3 crates/seedbox-arm64/compare.py \
  "$SEEDBOX_ARM64_REPORT_DIR/kvm-seed1-a.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-1a.json"
python3 crates/seedbox-arm64/compare-pages.py \
  "$SEEDBOX_ARM64_REPORT_DIR/cli-1a.pages.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-1b.pages.json"
for case in 1a 2; do
  for tool in compare compare-pages; do
    suffix=json
    if [ "$tool" = compare-pages ]; then suffix=pages.json; fi
    rc=0
    python3 "crates/seedbox-arm64/$tool.py" \
      "artifacts/arm64-irq-rendezvous/hvf-ram-pages/cli-$case.$suffix" \
      "$SEEDBOX_ARM64_REPORT_DIR/cli-$case.$suffix" \
      > "$SEEDBOX_ARM64_REPORT_DIR/$tool-$case.log" 2>&1 || rc=$?
    cat "$SEEDBOX_ARM64_REPORT_DIR/$tool-$case.log"
    printf 'comparison exit: %s (0=match, 1=different, 2=error)\n' "$rc"
    test "$rc" -le 1
  done
done
```

The comparison loop permits a differing result only to retain both seed logs;
it prints and preserves each status and does not declare equality. If these
pages still differ, use the printed GPAs with the matching kernel symbol map or
a separately authorized byte capture to distinguish CPU-data, printk, GIC state
and residual stack differences.

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
