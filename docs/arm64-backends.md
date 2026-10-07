# Experimental arm64 PV machine

`crates/seedbox-arm64` and `kernel-arm64` are isolated from the production x86
engine. The [README quickstart](../README.md#experimental-arm64-quickstart)
contains Mac setup and input verification. This is a single-CPU fixture backend,
not Compose application support.

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
be clear. Masked level assertions are retained in Machine and delivered at the
next unmasked semantic exit; deassertions cancel any queued assertion. Debug
unlock completions do not flush pending lines. The idle helper unmasks IRQs and
executes ISB before the MMIO rendezvous, then restores its incoming mask.
A polling yield restores and unmasks IRQs, executes ISB, writes event+24, then
masks again before retrying, provided the caller had IRQs enabled. A masked
caller stays masked and leaves delivery to a later rendezvous.
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

### Rebuild handoff after the review fix

Rebuild **both** the Linux Image and initramfs from the updated `kernel-arm64/`
directory. `seedbox.c` changes polling-yield IRQ handling; `init.c` adds a 20 ms
CLOCK_MONOTONIC busy-wait across at least one HZ=100 tick. Copy the entire updated
directory to the existing arm64 Linux build VM, replacing its old source copy.
Run there from the checkout root, with output on its Linux disk:

```sh
BUILD_JOBS=4 bash kernel-arm64/build.sh artifacts/arm64-review-yield
```

For a VM containing only the copied directory, run the same command from its
parent. The build requires GCC, binutils, musl-tools, flex, bison, bc, libssl-dev,
libelf-dev, curl, xz-utils and Python 3. It verifies the Linux 6.12.112 archive,
uses a fresh source/object tree, checks the resolved config and audits vmlinux
and the vDSO for counter/RNG leaks before exporting runnable inputs.

Copy back `Image`, `init.cpio`, `kernel.config`, `counter-audit.txt`, `SHA256SUMS`
and `build-directory.txt` into a fresh `artifacts/arm64-review-yield/` on the Mac.
Retain the old inputs separately. The operator must review the new input hashes,
complete acceptance, and fill both placeholders in
`kernel-arm64/expected-hashes.sha256`. Do not use the old witness as the expected
result of the changed fixture. Build-generated `SHA256SUMS` is only a copy check.

The review-fix handoff intentionally stops before this rebuild and new-guest
HVF/KVM boots. The old-input regression checks only unchanged behavior:

| Seed | Old-fixture HVF witness |
| --- | --- |
| 1 | `b3de4bbdaa7a747b54d3a940f02c4281bd3f26fe8574454c6cb7f06a3658a2a7` |
| 2 | `3477c8e474cbe111646f5a6aa03acadffefc5bb2995212bcd2aafec648e6dabe` |

Those inputs are Image SHA-256
`7ea8fa2858ef33d05da559e9de5e5a9329c6d397e5b03fd7f4fcd3ebb75d9ff1`
and init.cpio SHA-256
`9b952991625d7603e2db03e5ccdb87f78f1f7a16e6c664062505b7ddc75d8649`.
The ignored `hvf_old_fixture_regression` CLI test checks these input hashes,
both witnesses and trace hashes, exit code 0 and exact one-line stdout.

### Portable checks and Mac build

From the project Nix shell (`nix develop`, never `path:.`):

```sh
cargo fmt --manifest-path crates/seedbox-arm64/Cargo.toml --check
cargo clippy --manifest-path crates/seedbox-arm64/Cargo.toml --all-targets --locked -- -D warnings
cargo test-arm64
python3 kernel-arm64/test_inputs.py
python3 crates/seedbox-arm64/test_compare.py
cargo build-arm64
mkdir -p artifacts
cargo test-arm64 --test hardware --no-run --message-format=json > artifacts/arm64-tests.jsonl
arm64_test_binary="$(python3 -c 'import json; print(next(x["executable"] for x in map(json.loads, open("artifacts/arm64-tests.jsonl")) if x.get("reason") == "compiler-artifact" and x["target"]["name"] == "hardware" and x.get("executable")))')"
bash crates/seedbox-arm64/codesign.sh
bash crates/seedbox-arm64/codesign.sh "$arm64_test_binary"
```

Finish all Cargo builds before signing the CLI and test executable. Relinking
invalidates the entitlement signature; invoke signed tests directly.

### Acceptance after the rebuild

```sh
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-review-yield/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-review-yield/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$PWD/artifacts/arm64-review-yield/hvf-acceptance"
"$arm64_test_binary" --ignored --exact hvf_determinism --nocapture --test-threads=1
```

Use a fresh report directory. The suite runs seeds 1, 1, 2 and seed 1 with host
CPU load and randomized pre-entry sleeps. It compares complete witnesses,
checks seed sensitivity, guest sleep checks, the busy-wait marker and at least
one event+24 yield. The existing under-one-second undisturbed HVF timing gate
also remains to be re-evaluated against the new busy-wait fixture after rebuild.
The guest must print `SEEDBOX_INIT_PASS` and reach PSCI SYSTEM_OFF.

Then perform the README's two CLI runs with these rebuilt inputs: both must
exit 0 and print identical `WITNESS: <hex>` lines. `--witness FILE` additionally
writes a new JSON report; compare same-backend reports with:

```sh
python3 crates/seedbox-arm64/compare.py LEFT.json RIGHT.json
```

Linux arm64 KVM needs `/dev/kvm`, vGICv3 and the Linux 6.4+ SMCCC filter. Build the
isolated crate with `cargo build-arm64`, use `--backend kvm` with the same retained
inputs, and run `cargo test-arm64 --test hardware -- --ignored --exact kvm_determinism --nocapture`.
Export the same three `SEEDBOX_ARM64_*` variables with VM-local paths first.
No signing is required on Linux. Nested virtualization is needed only when
running KVM inside a VM, not when using a VM to build guest inputs.

Optional `--ram-pages FILE` writes final 4 KiB page hashes without changing the
witness. `compare-pages.py` locates divergent pages for diagnostics.

## Open items

- The new polling-yield kernel and busy-wait fixture require operator rebuild
  and native HVF/KVM acceptance. Portable tests do not establish that result.
- Full RAM witnesses are backend-specific. Earlier inputs had identical traces
  and console across HVF/KVM but 296 differing RAM pages. HVF advertises 1024 GIC
  IDs; the Linux 6.8 KVM limit is 992. Guest GIC count policy, MIDR and other CPU
  feature differences belong in a follow-up; this PR does not match RAM.
- Ubuntu cloud image and apt compiler/binutils versions are not pinned. The
  independent expected-input hashes detect drift; they do not make builds
  reproducible across toolchains.
- This fixture does not qualify networking, SMP, Compose applications, counted
  stops, syscall charging or guests that compute forever without yielding.
