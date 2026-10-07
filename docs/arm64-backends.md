# arm64 PV backends: first implementation milestone

This is an isolated experimental engine in `crates/seedbox-arm64`, based on
main `4a89dce`. It does not change the production x86 engine or its guest inputs.
The crate has its own workspace, lockfile and target directory. The only changes
to existing build files are the root workspace exclusion and the two aliases.

**Status: native HVF and operator-run nested KVM determinism acceptance PASS;
cross-backend equality still FAILS.** The same idle-rendezvous Image from
`180705e` boots reproducibly on both. This continuation makes the two debug
unlock writes transparent to scheduling and the witness. Signed HVF acceptance
still passes, including noise and all RAM. Comparing the new HVF report with
retained KVM evidence exposes a separate 1 ns guest-progress difference and
RAM mismatch; it does not establish cross-backend equality. See the
[continuation record](#transparent-debug-completion-and-cross-backend-analysis-2026-10-07).

Current Mac checks: `cargo test-arm64` passes 15 portable tests; the hardware
test remains ignored by default. Crate formatting and clippy with warnings
denied pass, as do seven Python input tests and the root portable acceptance
checks. The explicitly signed `hvf_determinism` test passes with the supplied
idle-rendezvous Image. Earlier failures remain recorded below.

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
set -euo pipefail
test -r /dev/kvm && test -w /dev/kvm
(cd artifacts/arm64-irq-rendezvous && sha256sum -c SHA256SUMS)
cargo test-arm64 --target aarch64-unknown-linux-gnu
cargo build-arm64 --target aarch64-unknown-linux-gnu
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-irq-rendezvous/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-irq-rendezvous/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$PWD/artifacts/arm64-irq-rendezvous/kvm-transparent-debug"
test ! -e "$SEEDBOX_ARM64_REPORT_DIR"
cargo test-arm64 --target aarch64-unknown-linux-gnu --test hardware \
  -- --ignored --exact kvm_determinism --nocapture --test-threads=1
for case in 1a 1b 2; do
  seed="${case%[ab]}"
  crates/seedbox-arm64/target/aarch64-unknown-linux-gnu/release/seedbox-arm64 run \
    --kernel "$SEEDBOX_ARM64_KERNEL" --initrd "$SEEDBOX_ARM64_INITRD" \
    --seed "$seed" --backend kvm --witness "$SEEDBOX_ARM64_REPORT_DIR/cli-$case.json"
done
python3 crates/seedbox-arm64/compare.py "$SEEDBOX_ARM64_REPORT_DIR/cli-1a.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-1b.json"
python3 crates/seedbox-arm64/compare.py "$SEEDBOX_ARM64_REPORT_DIR/kvm-seed1-a.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-1a.json"
python3 crates/seedbox-arm64/compare.py artifacts/arm64-irq-rendezvous/hvf-transparent-debug/hvf-seed1-a.json "$SEEDBOX_ARM64_REPORT_DIR/cli-1a.json"
```

The final cross-backend compare is expected to exit 1 with the retained
progress mismatch described below; preserve that output. The KVM suite performs
the same four determinism runs but does not impose the native-HVF wall-time bound. Retain failures, including setup errors, rather than
switching to a host timer or retrying until witnesses happen to match.

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

Run section 4 on the updated commit, keeping the same inputs and using the new
`kvm-transparent-debug` report directory. No guest rebuild is required. Because
KVM exposes neither debug write, its semantic execution is unchanged; its new
seed-1 witness is expected to remain `ed30d3bb…`, not become `f2394e95…`.
The hardware suite should still pass. The final cross-backend comparison is
expected to exit 1 at the PL011 read described above. Retain that result and
any new divergence; equality remains an open gate. Native HVF and nested KVM
repeatability are separate demonstrated properties.

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
