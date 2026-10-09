# Experimental arm64 PV machine

`crates/seedbox-arm64` and `kernel-arm64` are isolated from the production x86
engine. The [quickstart below](#experimental-arm64-quickstart)
contains Mac fixture setup and input verification. The library also provides
an application personality connected to the shared world coordinator. The public
Compose CLI selects this native backend on Apple Silicon; the standalone
quickstart below continues to run the isolated fixture.

## Application groundwork

The library now has `boot::BootSpec::for_application` to place a prepared
archive automatically for a caller-selected RAM size and command line.
`validate` produces an immutable `BootLayout` with checked guest-physical
spans and RAM-offset translation. `Machine::from_spec` checks the complete
layout and generated FDT before allocating RAM or creating a VM. It accepts
archives larger than the fixture's former 16 MiB gap. The current conservative
layout remains below 4 GiB guest physical address space; larger layouts need
an explicit backend/CPU address-width contract. These are memory-placement
checks, not a guest readiness claim. Placement follows the
[Linux ARM64 boot protocol](https://www.kernel.org/doc/html/latest/arch/arm64/booting.html).

`BootSpec::fixture`, `Machine::new` and `fdt::build` retain the historical
fixture addresses, command line and FDT bytes. Portable tests freeze those
bytes, reject overlapping or out-of-range inputs before VM creation, and
check an automatically placed 20 MiB archive with a mock backend.

`PvClock::set_horizon` provides the clock primitive for the application world
scheduler. A finite horizon limits publication, yields and idle jumps;
reaching it leaves future timers armed and produces no timer interrupt.
Invalid updates or guest timestamps beyond that horizon fail without
changing the clock. `None` retains the fixture's original policy. Callers
must stop the guest before changing its horizon.

`Machine::run_until` reconciles the initial handshake, republishes clock bounds
and returns a cooperative stop at the horizon without consuming a future timer.
The application personality adds virtio network, control console and optional
block devices. The shared coordinator serializes one native worker per machine
through stamped, validated stopped observations; no ARM scheduler loop is duplicated.
The [coordinator contract](world-coordinator.md) defines effect ordering, process
cleanup and unsupported snapshot/fork operations. Crash and
reboot retain the disk's surviving sectors and give each incarnation its own
seeded entropy. Host process scheduling never advances virtual time.

Application evidence records boot layout, kernel/archive hashes, timer policy
and actual control-channel transfers. Full-state mode covers RAM, CPU/SIMD/GIC,
clock, UART and device state. The world records its horizons and pending network
and control state; it cannot emit the fixture's format-1 report. The application
kernel uses a dedicated supervisor device accessible only to PID 1, with no raw
physical-memory mapping. Build it with `kernel-arm64/build.sh --application`.

The standalone CLI still runs only the isolated fixture. Same-input native HVF
fixture regression passes at `d9dc6f5`: all
eight CLI and hardware-suite boots match #120's complete same-seed witnesses,
including host load and randomized entry sleeps. The application layout and
finite horizon APIs retain portable/mock coverage. [PR #129](https://github.com/gardnervickers/seedbox/pull/129)
records the validation and its scope.

## Experimental arm64 quickstart

The isolated `seedbox-arm64` crate runs a single-CPU PV fixture on native Mac
HVF or Linux arm64 KVM. It is separate from the Compose/x86 CLI.

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
# Preserve the inputs on the Mac, then remove only this task's VM.
limactl stop seedbox-build
limactl delete --force seedbox-build
exit  # leave the Lima shell; return to nix develop in the checkout

for repeat in 1 2; do
  crates/seedbox-arm64/target/release/seedbox-arm64 run \
    --backend hvf --kernel artifacts/arm64-mac/Image \
    --initrd artifacts/arm64-mac/init.cpio --seed 1 > "artifacts/arm64-mac/run-$repeat.stdout" || exit "$?"
  cat "artifacts/arm64-mac/run-$repeat.stdout"
done
cmp artifacts/arm64-mac/run-1.stdout artifacts/arm64-mac/run-2.stdout
```

The acceptance test is that both runs exit 0 and print **identical** stdout
lines; `cmp` checks equality. A successful run has this format:

```text
WITNESS: 1fc6ae4c665ea2514938b41d116a3cb54a5ab974bc55c9993ab29cb8a3d207cf
```

This is the seed-1 witness for the reviewed inputs on the acceptance Mac.
Both repeats and the host-noise run match it; seed 2 has a different witness.
Historical witnesses below belong to their recorded inputs.

Console and timing go to stderr; setup/runtime errors exit 2. Sign again after
Cargo relinks the executable. A different host CPU profile can change guest RAM
and the witness; this result does not establish cross-host or KVM equality.

The hash checker explicitly compares `Image` and `init.cpio` with the
independently reviewed expected values. A mismatch means different inputs, often from a changed source
or toolchain, and can produce a different witness. Ubuntu's cloud image and apt
toolchain are not pinned. Same-input, same-host repeatability remains required.
See [the operator steps](#operator-steps) for
rebuild handoff, full acceptance and the remaining cross-backend RAM difference.

## Backend contract

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

`Backend::run` returns `RunStop::Guest(Stop)` or `RunStop::Interrupted`.
HVF cancellation and ARM KVM INTR/EINTR return to the machine owner without a held
instruction. The machine re-enters through its liveness hook without reading or
publishing PV time, injecting IRQs, spending the guest-exit budget, completing an
instruction or recording a witness event. The backend does not infer a watchdog
cause from cancellation, and queued cancellation may return without guest entry.
This plumbing introduces no asynchronous watchdog or ARM cycle-proof enablement.
Run the signed native queued-cancellation check with
`nix develop --command bash harness/check.sh mac-hvf`. It copies
the test binary before signing, bounds each native test executable, and removes
the signed copy afterward. Portable machine tests cover repeated interruptions
with a pending timer and the last available semantic exit-budget slot.

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

## Guest layout and PV ABI 2

The CLI fixture's FDT describes 128 MiB RAM, one CPU, GICv3, a PL011 with fixed 24 MHz input
clocks, PSCI/HVC, chosen/initrd/bootargs, and the PV node. It has no architectural
timer node. A domain-separated SHA-256 of the run seed supplies `rng-seed` and
`kaslr-seed`. KASLR is disabled for this first fixture, but the FDT retains the
seed property. NR_CPUS is 2 because that is Linux arm64's configuration minimum;
only CPU 0 exists in the FDT and `nosmp maxcpus=1` is supplied.

The following addresses are the historical fixture layout. The library's
application boot layout can select RAM size and relocate initrd/FDT within RAM;
device and PV addresses remain fixed.

| Guest physical address | Purpose |
| --- | --- |
| `0x08000000`, `0x080a0000` | GIC distributor, first redistributor |
| `0x09000000` | PL011; SPI 33 (DT SPI number 1) |
| `0x0a000000`, 16 KiB | Shared time page, outside normal RAM |
| `0x0a010000`, 16 KiB | Clockevent MMIO; SPI 40 (DT SPI number 8) |
| `0x40000000`, 128 MiB | Zero-filled RAM; Image at its header's text_offset |
| `0x46000000` | initrd, at most 16 MiB |
| `0x47000000` | FDT |

The time page contains little-endian u64 `now`, `last`, `bound`, `abi`,
`deferred_irq_pending` at offsets 0, 8, 16, 24, 32. Host owns all words except
`last`, which the guest owns. `abi` is 2 and the FDT compatible is
`seedbox,pv-time-v2`; ABI-1 guests must be rebuilt. The pending word is 1 when
any SPI assertion is deferred and 0 otherwise. The kernel serializes reads
with local IRQ masking: return now if newer, otherwise last+1, store last. At
a semantic exit Machine reconciles `max(now,last)` and republishes. Between exits,
computation costs zero time. Guest-local read progress becomes host-visible
only at the next exit, matching the shape of the x86 PV design.

Clockevent u64 writes: offset 0 arms an absolute deadline (zero cancels); 8
acknowledges and lowers the IRQ; 16 declares idle; 24 yields at the published
clock bound; 32 performs the ABI-2 handshake. Idle with an armed deadline jumps
to it and raises SPI 40 while stopped. New IRQ assertions require PSTATE.I to
be clear and no other source to be asserted. Machine retains other level
assertions until the active source deasserts and an unmasked semantic exit occurs;
it drains one pending source in interrupt-ID order. Deassertions cancel any
queued assertion. This prevents a second hardware-pending IRQ from skidding
after the first handler unmasks, which would change the interrupted PC and
Linux interrupt entropy state. Full-state evidence includes the asserted source. Debug
unlock completions do not flush pending lines. The idle helper unmasks IRQs and
executes ISB before the MMIO rendezvous, then restores its incoming mask.
A polling yield restores and unmasks IRQs, executes ISB, writes event+24, then
masks again before retrying, provided the caller had IRQs enabled. A masked
caller stays masked and ignores the pending word, allowing reads to progress.
When `deferred_irq_pending` is set, an unmasked caller takes that same yield
rendezvous even with `bound = u64::MAX`. Machine accepts this yield without an
armed deadline and flushes the deferred SPI only with PSTATE.I clear. It does
not advance to a future deadline if one has since been armed. Publication
clears the word after delivery or cancellation; it never lowers the bound just
to request deferred delivery.
Idle without a deadline or a pending interrupt is an error. Crossing a polling bound yields, expires the timer and
republishes, so a clock poll cannot run past a deadline without an exit.
UART accesses and firmware calls have no extra time charge. PL011 transmission
is synchronous; its empty TX FIFO asserts a level IRQ when enabled, and guest
mask writes deassert it. Receive stays empty.

This milestone omits syscall charging, counted stops, and fixed-point
spin detection. A guest computing forever without exits can still hang; use an
external operator timeout. A host timeout must fail, never advance virtual time.
The 5,000,000 raw-stop limit catches exit-producing runaway guests, including
repeated transparent debug writes. Report `exits` counts witnessed semantic stops.

Report JSON includes `backend` (`hvf` or `kvm`) as metadata. It is excluded from
the witness domain `arch=arm64 timebase=pv`. `compare.py` refuses comparisons
across backends or with older reports lacking this field. Witnesses include all
RAM and the shared page; no backend-specific bytes are filtered out.

## Operator steps

Acceptance history is in the [PR #120 description](https://github.com/gardnervickers/seedbox/pull/120).

### Portable and native checks

From the project Nix shell (`nix develop`, never `path:.`):

```sh
bash harness/check.sh portable
bash harness/check.sh mac-hvf
```

`mac-hvf` builds the ARM test
executables with `hvf-tests`, then signs disposable copies and invokes all of
them directly. It never needs an individual test name or `--ignored`.

### Run hardware acceptance

```sh
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-hres/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-hres/init.cpio"
bash harness/check.sh mac-guests
```

The runner creates and prints a fresh report directory and enables `guest-tests`.
The suite runs seeds 1, 1, 2 and seed 1 with host
CPU load and randomized pre-entry sleeps. It compares complete witnesses,
checks seed sensitivity, guest sleep checks, a 200–300 us timerfd spin with an
observed expiration, and at least one event+24 yield. Deferral counts are
informational only; the mock review suites prove masked-then-unmasked delivery.
The undisturbed HVF timing budget is one second, including the 200 us timerfd
spin. Witness, RAM, seed-sensitivity and guest sleep checks remain unchanged.
The guest must print `SEEDBOX_INIT_PASS` and reach PSCI SYSTEM_OFF.

Then perform the quickstart's two CLI runs with these rebuilt inputs: both must
exit 0 and print identical `WITNESS: <hex>` lines. `--witness FILE` additionally
writes a new JSON report; compare same-backend reports with:

```sh
python3 crates/seedbox-arm64/compare.py LEFT.json RIGHT.json
```

### Linux VM KVM handoff (existing Cargo, no Nix)

Linux arm64 KVM needs `/dev/kvm`, vGICv3 and the Linux 6.4+ SMCCC filter.
Transfer the updated checkout, Image and init.cpio to the VM. From the
checkout root, using the VM's existing Cargo:

```sh
set -eu
export PATH="$HOME/.cargo/bin:$PATH"
cargo build-arm64
cargo test-arm64 --test review_yield_masked --test review_deferred_bound
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-hres/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-hres/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$(mktemp -d "$PWD/artifacts/arm64-hres/kvm-acceptance.XXXXXX")"
bash harness/check.sh linux-guests

for run in 1-a 1-b 2-a 2-b; do
  crates/seedbox-arm64/target/release/seedbox-arm64 run \
    --backend kvm --kernel "$SEEDBOX_ARM64_KERNEL" \
    --initrd "$SEEDBOX_ARM64_INITRD" --seed "${run%%-*}" \
    --witness "$SEEDBOX_ARM64_REPORT_DIR/cli-$run.json" \
    > "$SEEDBOX_ARM64_REPORT_DIR/cli-$run.stdout" \
    2> "$SEEDBOX_ARM64_REPORT_DIR/cli-$run.stderr" || exit "$?"
done
python3 crates/seedbox-arm64/compare.py \
  "$SEEDBOX_ARM64_REPORT_DIR/cli-1-a.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-1-b.json"
python3 crates/seedbox-arm64/compare.py \
  "$SEEDBOX_ARM64_REPORT_DIR/cli-2-a.json" "$SEEDBOX_ARM64_REPORT_DIR/cli-2-b.json"

python3 - <<'PYREPORT'
import json, os
from pathlib import Path
for path in sorted(Path(os.environ["SEEDBOX_ARM64_REPORT_DIR"]).glob("*.json")):
    report = json.loads(path.read_text())
    print(f"{path.name}: witness={report['witness']} exits={report['exits']} "
          f"wall_seconds={report['wall_seconds']:.6f}")
    if report["seed"] == 1:
        print(f"  trace_hash={report['trace_hash']}")
        print(f"  console_hash={report['console_hash']}")
PYREPORT
```

The hardware suite adds CPU load and randomized pre-entry sleeps. If the VM
user lacks `/dev/kvm` access, run the hardware test and CLI via `sudo env`,
passing the three exported paths and the user's Cargo PATH explicitly; do not
install another toolchain as root. No signing is required on Linux. Nested
virtualization is needed only when running KVM inside a VM, not when using a VM
to build guest inputs. Compare KVM reports with KVM reports: cross-backend full
RAM equality remains open.

Optional `--ram-pages FILE` writes final 4 KiB page hashes without changing the
witness. `compare-pages.py` locates divergent pages for diagnostics.

## Open items

- Full RAM witnesses are backend-specific. Earlier inputs had identical traces
  and console across HVF/KVM but 296 differing RAM pages. HVF advertises 1024 GIC
  IDs; the Linux 6.8 KVM limit is 992. Guest GIC count policy, MIDR and other CPU
  feature differences belong in a follow-up; this PR does not match RAM.
- Ubuntu cloud image and apt compiler/binutils versions are not pinned. The
  independent expected-input hashes detect drift; they do not make builds
  reproducible across toolchains.
- This fixture does not qualify networking, SMP, Compose applications, counted
  stops, syscall charging or guests that compute forever without yielding.
