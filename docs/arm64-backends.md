# Experimental arm64 PV machine

`crates/seedbox-arm64` and `kernel-arm64` are isolated from the production x86
engine. The [quickstart below](#experimental-arm64-quickstart)
contains Mac setup and input verification. This is a single-CPU fixture backend,
not Compose application support.

## Experimental arm64 quickstart

The isolated `seedbox-arm64` crate runs a single-CPU PV fixture on native Mac
HVF or Linux arm64 KVM. It is separate from the Compose/x86 CLI.

The checked-in `kernel-arm64/expected-hashes.sha256` retains the reviewed kernel
hash and marks `init.cpio` **rebuild required** for the high-resolution-mode
prerequisite. The operator's previous `aa73b83` initramfs fails at 300,002 ns
without an observed timerfd expiry. There is no accepted timerfd witness yet.
See the [diagnosis and initramfs-only rebuild](#timerfd-startup-diagnosis-and-rebuild-2026-10-07)
and the retained [failure record](#timerfd-guest-acceptance-2026-10-07).
Do not derive expected hashes from the files being checked. Build-generated
`SHA256SUMS` only checks transfer integrity.

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
python3 kernel-arm64/check-hashes.py artifacts/arm64-mac || exit "$?"
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
WITNESS: <sha256-from-a-successful-run>
```

There is no replacement witness for the current timerfd inputs: both runs
currently exit 2 with empty stdout. The former SIGALRM witness belongs only to
the [historical ABI-2 acceptance record](#historical-abi-2-guest-acceptance-2026-10-07)
and must not be used as an expectation for this fixture.

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
be clear. Masked level assertions are retained in Machine and delivered at the
next unmasked semantic exit; deassertions cancel any queued assertion. Debug
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

### Timerfd guest acceptance (2026-10-07)

The operator rebuilt the guest from `aa73b83`. The Mac independently verified
these supplied hashes before execution; the expected-input manifest at
`cc7e413` pinned them. Hash verification establishes input identity, not hardware
acceptance.

| Input | SHA-256 |
| --- | --- |
| Image | `141289b842d3a42ca13a634598372a7e66605da70d36c2f2c1cd5a2806bfb01f` (unchanged) |
| init.cpio | `ea7c7c5cc16ad72cd5b85e260edaf89cf329a43fc6677b185345f37c1c7bca2e` |
| kernel.config | `85f8af523f252b63298a7943b2f5cd57ecbfb7c040cc1d3146b44da05a9caf0a` (unchanged) |

The retained config has `CONFIG_HIGH_RES_TIMERS=y`, `CONFIG_TICK_ONESHOT=y`,
`CONFIG_TIMERFD=y` and HZ=100. `check-config.py` now explicitly requires
`CONFIG_TIMERFD=y` even if the fragment omits it. The operator-supplied counter
audit reports no hardware counter/RNG reads in vmlinux or the vDSO.

The fixture arms a one-shot `timerfd_create(CLOCK_MONOTONIC)` timer for
200,000 ns, then alternates clock reads with nonblocking timerfd reads. It
requires exactly one expiration observed while spinning and logs
`busy-wait elapsed=N`, with N in 200,000–300,000 ns. Missing the expiry fails
init. The undisturbed HVF wall gate remains **1 second**.

`deferred_spi_events` and `deferred_clock_read_spis` are diagnostic report
metadata, excluded from the witness and never pass gates. Failed runs also
include these counts in their stderr diagnostic. Successful CLI stdout stays
exactly one `WITNESS:` line. Portable `review_deferred_bound` and
`review_yield_masked` tests prove deferral, cancellation and delivery after
unmasking; hardware acceptance checks the guest's observed timerfd expiration.

**HVF acceptance fails with these inputs.** Native ad-hoc-signed CLI and hardware
test binaries were run on arm64 macOS 27.0.1 (26A434). Every CLI boot prints
`busy-wait elapsed=300002`, then `SEEDBOX_INIT_FAIL: PV timerfd duration`, and
powers off at virtual time 300,909 ns. No boot prints `SEEDBOX_INIT_PASS`.
Stdout is empty and no successful JSON report is written, so all five witnesses
and seed-1 `trace_hash` / `console_hash` are unavailable.

| CLI boot | Witness | Exit status | Semantic exits | Wall seconds |
| --- | --- | --- | --- | --- |
| Seed 1 A | unavailable (guest failed) | 2 | 1633 | 0.066466 |
| Seed 1 B | unavailable (guest failed) | 2 | 1633 | 0.056714 |
| Seed 1 with host CPU load | unavailable (guest failed) | 2 | 1633 | 0.057524 |
| Seed 2 A | unavailable (guest failed) | 2 | 1633 | 0.054637 |
| Seed 2 B | unavailable (guest failed) | 2 | 1633 | 0.054653 |

Every boot reports `deferred_spi_events=0` and `deferred_clock_read_spis=0`.
These numbers are informational; failure is the guest timerfd check.
The table is the final signed run after adding failure-counter diagnostics,
retained in `artifacts/arm64-hrtimer/hvf-cli-diagnostics/`. The original five
runs in `artifacts/arm64-hrtimer/hvf-cli/` failed identically; their wall times
were 0.067172, 0.056759, 0.058581, 0.054889 and 0.055213 seconds respectively.

The signed `hvf_determinism` test exits 101 (0 passed, 1 failed), aborting on
its first seed-1 boot: 1633 exits, 0.063493 seconds, the same guest failure and
zero deferral counts. Its remaining repeat/seed/jitter boots and one-second
wall assertion are **not reached**; a short failed boot does not pass that
gate. The original attempt also failed at the first boot (0.068290 seconds).
Logs are `artifacts/arm64-hrtimer/hvf-determinism-diagnostics.log` and
`artifacts/arm64-hrtimer/hvf-determinism.log`. All artifact paths are local and
ignored by Git.

A separate signed backend-forwarding probe records only the initial nonzero
clockevent deadline, 10,000,011 ns, with no timer SPI assertion and no event+24
polling yield before shutdown. Its source and log are retained in
`artifacts/arm64-hrtimer/startup-probe.rs` and `startup-probe.stderr`.
The follow-up below reproduces this failure, verifies the published page bound,
and fixes the fixture's missing high-resolution-mode prerequisite. These original
failure artifacts remain unchanged.

Both review suites pass (three tests each), as do arm64 tests, input/comparison
Python tests, arm64 and seedbox fmt/clippy, and root portable acceptance
including workspace tests. Linux-only VMM tests and new-input KVM acceptance
remain operator work.

### Timerfd startup diagnosis and rebuild (2026-10-07)

Rebuilt and ad-hoc-signed the unchanged VMM at `cc7e413`, verified the supplied
input hashes, and ran seed 1 twice and seed 2 once. Each run exits 2 with empty
stdout, 1633 semantic exits, `busy-wait elapsed=300002`, and both deferral counts
zero. Wall times are 0.066337, 0.057413 and 0.055093 seconds. These are failed
boots, not performance acceptance. Logs are in
`artifacts/arm64-timer-diagnosis/seed{1-a,1-b,2-a}.{stdout,stderr}`.

An independently signed forwarding backend logs every raw exit, PSTATE and
shared-page words without changing them (`probe.rs` / `probe.stderr` in the
same directory). The relevant sequence is:

- Raw exit 1203 writes event+0 = 10,000,011 with PSTATE 0xc5.
- Before raw exit 1204, publication is `now=11 last=11 bound=10000011 deferred=0`.
- No later deadline write, event+24 yield, or SPI 40 assertion occurs.
- Raw exit 1635 is SYSTEM_OFF, with `now=300909 last=300909 bound=10000011 deferred=0`.
  Two transparent debug exits explain the raw/semantic count difference.

The bound is published correctly and never crossed. There is consequently no
expired timer for the VMM to assert or defer. Clock reads advance `last`; the
patched vDSO falls back to syscalls and the PV clocksource calls `seedbox_read`.
The init loop reads timerfd on every iteration, before testing the spin bound.
Neither vDSO bypass nor a readiness check postponed until after spinning
explains this run.

The root cause is starting the sub-tick acceptance before Linux activates
high-resolution mode. In the exact
[Linux 6.12.112 hrtimer source](https://github.com/gregkh/linux/blob/v6.12.112/kernel/time/hrtimer.c),
resolution starts at `LOW_RES_NSEC`; `__hrtimer_reprogram` returns while high
resolution is inactive. `hrtimer_run_queues`, called each tick, performs the
switch and sets resolution to 1 ns. The first 10 ms tick has not occurred in
this 0.301 ms boot. `CONFIG_HIGH_RES_TIMERS=y` makes the mode available but does
not activate it before that tick.

The fix moves the existing 10 ms sleep check before `busy_wait_check`, then
requires `clock_getres(CLOCK_MONOTONIC)` to report 1 ns before timerfd is armed.
[Linux's clock_getres implementation](https://github.com/gregkh/linux/blob/v6.12.112/kernel/time/posix-timers.c)
returns `hrtimer_resolution` for this clock. A failed transition now produces
`PV high-resolution timer mode not active`. The measured spin still contains
no sleep, checks timerfd every iteration and requires exactly one expiration
in **200,000–300,000 ns**. The 100 us allowance covers deterministic syscall/IRQ
clock reads after arming; accepting a 10 ms expiry would fail to exercise a
sub-tick high-resolution timer. The one-second HVF wall gate is unchanged.
No VMM or kernel driver change is needed; SPI assertions remain forbidden
while PSTATE.I is set.

Validation passes: `cargo test-arm64` (including all three tests in each of
`review_yield_masked` and `review_deferred_bound`), 11 guest-input tests
(including rejection of low-resolution mode and clock_getres errors), report
comparison tests, arm64/seedbox fmt and clippy, workspace tests and root
portable acceptance. Linux-only VMM tests do not run on this Mac.

**STOP: rebuild only `init.cpio`; revised hardware acceptance is pending.**
Keep the supplied Image (`141289b842d3a42ca13a634598372a7e66605da70d36c2f2c1cd5a2806bfb01f`),
`kernel.config` and counter audit. In the existing Linux arm64 build VM, copy
the updated `kernel-arm64/` directory and run from the checkout root using the
same musl toolchain as `build.sh`:

```sh
set -eu
init_rebuild_dir="$(mktemp -d /tmp/seedbox-arm64-init.XXXXXX)"
init_source_dir="$(pwd)/kernel-arm64"
musl-gcc -static -Os -fno-ident \
  -ffile-prefix-map="$init_source_dir"=/seedbox/kernel-arm64 \
  -Wl,--build-id=none -Wall -Wextra -Werror \
  "$init_source_dir/init.c" -o "$init_rebuild_dir/init"
python3 kernel-arm64/build-initramfs.py \
  "$init_rebuild_dir/init" "$init_rebuild_dir/init.cpio"
sha256sum "$init_rebuild_dir/init.cpio"
printf 'rebuilt initramfs: %s/init.cpio\n' "$init_rebuild_dir"
```

Copy the new initramfs alongside the retained Image in a fresh Mac artifact
directory. Independently review its hash before replacing the manifest's
`REBUILD_REQUIRED_INITRD_SHA256` placeholder. The old hash remains in the
failure record above. A full `BUILD_JOBS=4 bash kernel-arm64/build.sh OUTPUT`
is also supported but rebuilding Image is unnecessary for this change.
Then run the signed CLI for seed 1 twice, seed 1 with host noise and seed 2
twice, plus `hvf_determinism`. Require same-seed complete witness equality,
`clock resolution=0.000000001`, observed expiry in the unchanged window,
seed-1 trace/console hashes, and the unchanged wall-time gate. There are no
new successful witnesses until that rebuild and hardware run occur.

### Historical ABI-2 guest acceptance (2026-10-07)

The operator rebuilt **both** `Image` and `init.cpio` from `f9d7e6a` in the
Linux VM. The Mac independently checked the supplied hashes before execution:

| Input | SHA-256 |
| --- | --- |
| Image | `141289b842d3a42ca13a634598372a7e66605da70d36c2f2c1cd5a2806bfb01f` |
| init.cpio | `7f73307576b5ff352ccc9920bd3cb7178ed6dc60d42e2f9360f39e2efbf539df` |
| kernel.config | `85f8af523f252b63298a7943b2f5cd57ecbfb7c040cc1d3146b44da05a9caf0a` (unchanged) |

The expected hashes at that revision identified these ABI-2 inputs. The supplied
counter audit reports no hardware counter/RNG reads in vmlinux or the vDSO.
For subsequent rebuilds, copy the entire updated `kernel-arm64/` directory to
the Linux VM and run `BUILD_JOBS=4 bash kernel-arm64/build.sh OUTPUT_DIRECTORY`.
Retain `Image`, `init.cpio`, `kernel.config`, `counter-audit.txt`, `SHA256SUMS`
and `build-directory.txt`. Build-generated hashes check transfer integrity;
new expected hashes still require independent review.

Native ad-hoc-signed CLI and hardware-test binaries were built from `f9d7e6a`
and run on arm64 macOS 27.0.1 (26A434). Witnesses include all RAM and the shared
page:

- Seed 1 witness: `028d2b7c14c1e6786c38cb78013f51707e5fc23400ac9d9ed95b2b491dec26a1`.
- Seed 2 witness: `8f3fb1946a6768643beb67e5fb503fc5be9c1748d2779d754fedcbe07631b40c`.
- Seed 1 trace_hash: `5bee9936c5a598d4fbcb930967e493e9ca2cc7ad4df89c26aeb00c68ff51dfda`.
- Seed 1 console_hash: `fb767b035bfe2d3f282e0910c070221c37723e140ce3a2a0841b4f4e04afdf07`.

| CLI boot | Witness | Exit status | Semantic exits | Wall seconds |
| --- | --- | --- | --- | --- |
| Seed 1 A | Seed 1 above | 0 | 2355 | 1.065410 |
| Seed 1 B | Seed 1 above | 0 | 2355 | 1.068000 |
| Seed 1 with host CPU load | Seed 1 above | 0 | 2355 | 1.114860 |
| Seed 2 A | Seed 2 above | 0 | 2355 | 1.059422 |
| Seed 2 B | Seed 2 above | 0 | 2355 | 1.063885 |

All five print one witness line and `SEEDBOX_INIT_PASS`, record two event+24
polling yields, and reach PSCI SYSTEM_OFF at virtual time 1,020,101,052 ns.
The new 200 us `setitimer(ITIMER_REAL)` / SIGALRM spin completes with
`busy-wait elapsed=10000128` (10.000128 ms of guest time). The 10 ms and 1 s
sleep checks report 10,050,007 ns and 1,000,050,007 ns. The spin's requested
alarm interval is 200 us; its measured completion is not 200 us. Same-seed
complete report comparisons pass, including the CPU-load boot; seed 2 differs.

The signed `hvf_determinism` suite passes its complete-witness, seed-sensitivity,
guest sleep and polling-yield assertions, then **fails the unchanged one-second
wall gate**, exiting 101 (0 passed, 1 failed): baseline 1.067484 seconds.
No retry or timing-budget change was made.

| Signed hardware-suite boot | Witness | Semantic exits | Wall seconds |
| --- | --- | --- | --- |
| Seed 1 A | Seed 1 above | 2355 | 1.067484 |
| Seed 1 B | Seed 1 above | 2355 | 1.065437 |
| Seed 2 | Seed 2 above | 2355 | 1.067610 |
| Seed 1 with CPU load and randomized pre-entry sleeps | Seed 1 above | 2355 | 9.733929 |

Every hardware-suite guest powers off successfully. The jitter boot is outside
the baseline timing gate. All seed-1 CLI and suite trace/console hashes match
the values above. Evidence is retained under `artifacts/arm64-abi2/hvf-cli/`,
`artifacts/arm64-abi2/hvf-acceptance/` and
`artifacts/arm64-abi2/hvf-determinism.log` (ignored local artifacts).

All three `review_yield_masked` and all three `review_deferred_bound` tests
pass. The actual guest polling-helper C checks, arm64 tests, input/comparison
Python tests, arm64 and seedbox fmt/clippy, and root portable acceptance
(including workspace tests) pass. Linux-only VMM tests and ABI-2 KVM acceptance
remain operator work. HVF determinism is demonstrated; the one-second
performance acceptance remains unmet.

### Historical ABI-1 guest acceptance (2026-10-07)

Old-input witness tests require the ABI-1 VMM at `66cf845`; current guest/VMM
inputs must use ABI 2 together.

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
were checked to return zero for that override. At this historical acceptance,
a complete Linux rebuild with the script change had not been run here;
these ABI-1 artifacts remain exactly
those supplied by the operator. The ABI-2 rebuild above subsequently retained
the same config hash. GCC/binutils and cloud-image pinning remain open.

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

### Run hardware acceptance

```sh
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-hrtimer/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-hrtimer/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$(mktemp -d "$PWD/artifacts/arm64-hrtimer/hvf-acceptance.XXXXXX")"
"$arm64_test_binary" --ignored --exact hvf_determinism --nocapture --test-threads=1
```

Use a fresh report directory. The suite runs seeds 1, 1, 2 and seed 1 with host
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
Transfer the updated checkout to the VM and retain the **same** accepted Image
and init.cpio. From the checkout root, using the VM's existing Cargo:

```sh
set -eu
export PATH="$HOME/.cargo/bin:$PATH"
python3 kernel-arm64/check-hashes.py artifacts/arm64-hrtimer
cargo build-arm64
cargo test-arm64 --test review_yield_masked --test review_deferred_bound
export SEEDBOX_ARM64_KERNEL="$PWD/artifacts/arm64-hrtimer/Image"
export SEEDBOX_ARM64_INITRD="$PWD/artifacts/arm64-hrtimer/init.cpio"
export SEEDBOX_ARM64_REPORT_DIR="$(mktemp -d "$PWD/artifacts/arm64-hrtimer/kvm-acceptance.XXXXXX")"
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

- The independently verified `aa73b83` timerfd guest fails at 300,002 ns,
  before observing an expiry; all five HVF CLI boots exit 2 and the signed
  hardware suite exits 101 before its wall gate. Resolve the initial tick /
  high-resolution activation issue, rebuild and independently review inputs,
  then rerun full HVF acceptance with the unchanged one-second gate. Linux
  arm64 KVM acceptance is pending; record its same-backend repeats plus seed-1
  trace_hash and console_hash against the identical revised inputs. Equal
  exit counts alone do not show equality.
- Full RAM witnesses are backend-specific. Earlier inputs had identical traces
  and console across HVF/KVM but 296 differing RAM pages. HVF advertises 1024 GIC
  IDs; the Linux 6.8 KVM limit is 992. Guest GIC count policy, MIDR and other CPU
  feature differences belong in a follow-up; this PR does not match RAM.
- Ubuntu cloud image and apt compiler/binutils versions are not pinned. The
  independent expected-input hashes detect drift; they do not make builds
  reproducible across toolchains.
- This fixture does not qualify networking, SMP, Compose applications, counted
  stops, syscall charging or guests that compute forever without yielding.
