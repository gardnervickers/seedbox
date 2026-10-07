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

### Review-fix guest build

Build **both** the Linux Image and initramfs from the updated `kernel-arm64/`
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
Retain the old inputs separately. The checked-in expected hashes now contain the
operator-supplied values for the accepted `90fb080` guest. Verify them with
`python3 kernel-arm64/check-hashes.py artifacts/arm64-review-yield`.
Build-generated `SHA256SUMS` is only a copy check. The old-input regression checks
only unchanged behavior:

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

### Rebuilt guest acceptance (2026-10-07)

Inputs rebuilt by the operator from `90fb080` and independently hash-checked:

| Input | SHA-256 |
| --- | --- |
| Image | `4f2846f027b03fcfd859bb222456779be1967b21b4e4dc0c0e59b978ec3a9be5` |
| init.cpio | `4f7dcbcb7b3705799913dd16fcdf4297216f514476837a7062cda48b740f22d4` |
| kernel.config | `85f8af523f252b63298a7943b2f5cd57ecbfb7c040cc1d3146b44da05a9caf0a` |

The config differs from the previous
`facb82e4dbad040a19a553304413975b72e6c8eeb8b511414729a7c21a71a822`
only in host Rust detection: `CONFIG_RUSTC_VERSION` changes from `109900` to
`0`, `CONFIG_RUSTC_LLVM_VERSION` from `230101` to `0`, and
`CONFIG_RUSTC_HAS_UNNECESSARY_TRANSMUTES=y` and
`CONFIG_RUSTC_HAS_SUSPICIOUS_RUNTIME_SYMBOL_DEFINITIONS=y` disappear. No guest
feature selection changed. Linux 6.12.112's `init/Kconfig` probes the Rust
compiler independently of `CONFIG_RUST`; neither config enables Rust or embeds
its config (`CONFIG_IKCONFIG=n`). This was environment-dependent config drift,
not an intended config change. The build script now passes `RUSTC=false` to
**every** make invocation to pin absent Rust for this C-only guest, including
when `$HOME/.cargo/bin` is on PATH. The pinned source archive's version probes
were checked to return zero for that override. A complete Linux rebuild with
the script change has not been run here; the accepted artifacts remain exactly
those supplied by the operator. GCC/binutils and cloud-image pinning remain open.

Native signed CLI results on arm64 macOS 27.0.1 (26A434), with full RAM and the
shared page included:

- Seed 1 witness: `4b7408386842bbaf2c705475af2502a6c294b0a418c70107c1b9582b4abb3505`.
- Seed 2 witness: `12ab4033745c10261da3c518cf103a47c2c8bf0ba1a5863e1e23536ea1465cb5`.

| Boot | Witness | Exit status | Semantic exits | Wall seconds |
| --- | --- | --- | --- | --- |
| Seed 1 A | Seed 1 above | 0 | 2359 | 2.083875 |
| Seed 1 B | Seed 1 above | 0 | 2359 | 2.019816 |
| Seed 1 with host CPU load | Seed 1 above | 0 | 2359 | 1.980087 |
| Seed 2 A | Seed 2 above | 0 | 2359 | 1.902880 |
| Seed 2 B | Seed 2 above | 0 | 2359 | 1.935355 |

All five print exactly one witness line, `busy-wait elapsed=20000000` and
`SEEDBOX_INIT_PASS`, record three event+24 polling yields, and reach PSCI
SYSTEM_OFF at virtual time 1,030,100,924 ns. The 10 ms and 1 s sleep checks
report 10,050,007 ns and 1,000,050,007 ns respectively. Same-seed report
comparisons pass, including the CPU-load boot, and seed 2 differs.

Evidence is retained under `artifacts/arm64-review-yield/hvf-cli/` and
`artifacts/arm64-review-yield/hvf-acceptance/` (ignored local artifacts).
The first signed `hvf_determinism` run matched all witnesses, including its
CPU-load plus randomized pre-entry-sleep run, and passed busy-wait/yield checks,
but exited 101 on the old one-second baseline timing gate: 2.175578 seconds.
The other boots were 2.144574, 2.257381 and 10.122795 seconds (jitter), all with
2359 exits. That failure is retained in `hvf-determinism.log`.

After updating only the expanded fixture's wall-time budget to five seconds,
the rebuilt and re-signed `hvf_determinism` test exits 0 (1 passed). Complete
witnesses match the CLI results above; every boot has 2359 semantic exits:

| Signed hardware-suite boot | Wall seconds |
| --- | --- |
| Seed 1 A | 1.884785 |
| Seed 1 B | 1.911778 |
| Seed 2 | 1.886465 |
| Seed 1 with CPU load and randomized pre-entry sleeps | 9.267590 |

The noisy boot is outside the undisturbed timing gate. Its witness comparison
is unchanged. Final reports and the passing log are in `hvf-acceptance-final/`
and `hvf-determinism-final.log` under the same artifact directory.

The three `review_yield_masked` regressions and the actual guest polling-helper
C test pass: masked assertions are deferred, cancellation removes pending IRQs,
transparent debug completions cannot flush them, and the next unmasked semantic
exit delivers each retained timer/UART assertion once. Deterministic comparison
and masked-IRQ rules are unchanged. Root portable acceptance, workspace tests,
arm64 tests, both fmt/clippy checks and input/comparison Python tests pass.
Linux-only VMM tests and rebuilt-guest KVM execution remain operator work.

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
one event+24 yield. The undisturbed HVF timing budget is five seconds for this
expanded fixture. Its 20 ms PV busy-wait advances through 1 ns clock reads and
takes about two seconds of host CPU time; the former one-second sleep-only
budget no longer fits. This is a performance budget change only; no witness,
RAM, seed-sensitivity, guest sleep or IRQ assertion is relaxed.
The guest must print `SEEDBOX_INIT_PASS` and reach PSCI SYSTEM_OFF.

Then perform the README's two CLI runs with these rebuilt inputs: both must
exit 0 and print identical `WITNESS: <hex>` lines. `--witness FILE` additionally
writes a new JSON report; compare same-backend reports with:

```sh
python3 crates/seedbox-arm64/compare.py LEFT.json RIGHT.json
```

### Linux VM KVM handoff (existing Cargo, no Nix)

Linux arm64 KVM needs `/dev/kvm`, vGICv3 and the Linux 6.4+ SMCCC filter.
Transfer the updated checkout to the VM and retain the **same** accepted Image
and init.cpio. From the checkout root, using the VM's existing Cargo:

```sh
set -eu
export PATH="$HOME/.cargo/bin:$PATH"
python3 kernel-arm64/check-hashes.py artifacts/arm64-review-yield
cargo build-arm64
cargo test-arm64 --test review_yield_masked
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-review-yield/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-review-yield/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$(mktemp -d "$PWD/artifacts/arm64-review-yield/kvm-acceptance.XXXXXX")"
cargo test-arm64 --test hardware -- --ignored --exact kvm_determinism --nocapture --test-threads=1

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

- The rebuilt polling-yield kernel and busy-wait fixture still require native
  Linux arm64 KVM acceptance. HVF results do not establish KVM behavior.
- Full RAM witnesses are backend-specific. Earlier inputs had identical traces
  and console across HVF/KVM but 296 differing RAM pages. HVF advertises 1024 GIC
  IDs; the Linux 6.8 KVM limit is 992. Guest GIC count policy, MIDR and other CPU
  feature differences belong in a follow-up; this PR does not match RAM.
- Ubuntu cloud image and apt compiler/binutils versions are not pinned. The
  independent expected-input hashes detect drift; they do not make builds
  reproducible across toolchains.
- This fixture does not qualify networking, SMP, Compose applications, counted
  stops, syscall charging or guests that compute forever without yielding.
