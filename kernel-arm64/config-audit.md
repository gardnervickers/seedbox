# Linux 6.12.112 arm64 configuration audit

Archive: `linux-6.12.112.tar.xz` from `https://cdn.kernel.org/pub/linux/kernel/v6.x/`.
SHA-256: `164dc9d1f6c93c61a15e1f071c48379b467f2b17c469cce7223471968208ed03`.

The archive was downloaded and its SHA-256 verified on the Mac. Every fragment
symbol was searched in `arch/arm64` and shared `Kconfig*` files; the definitions,
dependencies, selections and enclosing menus were inspected. Both removed
`RANDOM_TRUST_*` symbols have no definition. Linux `drivers/char/random.c`
registers `random.trust_cpu` and `random.trust_bootloader` as early parameters;
`crates/seedbox-arm64/src/fdt.rs` already supplies `off` and `on`, respectively.

The patched tree passed native Darwin-host Kconfig compilation and actual
`allnoconfig`, `scripts/kconfig/merge_config.sh -m`, and `olddefconfig`.
The initial default `CC=gcc` attempt failed because Apple's assembler is not
supported. Using the project shell's Clang 21.1.8 for both host and target,
with installed Nix LLD 21.1.8, succeeded. Kbuild adds the target triple
`aarch64-linux-gnu`; no compiler capability probes were stubbed. Commands:

```sh
# src is the extracted tree after applying kernel-arm64/apply.py.
# out is an absolute path; ld_lld is the installed Nix ld.lld executable.
make -C "$src" O="$out" ARCH=arm64 CC=clang HOSTCC=cc LD="$ld_lld" allnoconfig
bash "$src/scripts/kconfig/merge_config.sh" -m -O "$out" "$out/.config" kernel-arm64/config.fragment
make -C "$src" O="$out" ARCH=arm64 CC=clang HOSTCC=cc LD="$ld_lld" olddefconfig
python3 kernel-arm64/check-config.py "$src" kernel-arm64/config.fragment "$out/.config"
```

All 72 settings matched. Kconfig evaluated the full dependency graph, including
hidden symbols and enclosing conditions. The table records source locations and
resolved values; `n (absent)` means a defined symbol hidden by disabled dependencies.
Explicit disabled requests remain intentional constraints, even when already off
in allnoconfig. The merge script's redefinition messages for `=n` are harmless.

Timerfd fixture follow-up: the fragment also explicitly requests `TIMERFD=y`.
The retained `artifacts/arm64-abi2/kernel.config` already has `CONFIG_TIMERFD=y`,
`CONFIG_HIGH_RES_TIMERS=y`, `CONFIG_TICK_ONESHOT=y` and `CONFIG_HZ=100`.
This pins an existing selection; the next Linux build will recheck all fragment
requests through `check-config.py`. No new Kconfig build is claimed here.

The operator's `23c619e` rebuild retains those selections in
`artifacts/arm64-hres/kernel.config` (SHA-256
`85f8af523f252b63298a7943b2f5cd57ecbfb7c040cc1d3146b44da05a9caf0a`).
The checker now also requires `CONFIG_HIGH_RES_TIMERS=y` and `CONFIG_TIMERFD=y`
directly in the resolved config, independently of fragment requests. Portable
regressions reject missing, disabled and modular values for either prerequisite.
Signed HVF execution confirms 1 ns clock resolution and an observed timerfd
expiration during a 200,009 ns spin; see `docs/arm64-backends.md`.

Key dependency chains checked:

- ARM64 selects ARM_AMBA, ARM_GIC_V3, ARM_PSCI_FW, COMMON_CLK, OF,
  GENERIC_CLOCKEVENTS_BROADCAST and GENERIC_TIME_VSYSCALL; the patch removes only
  the ARM_ARCH_TIMER selection. 64BIT, MMU and SMP are always enabled on arm64.
- NR_CPUS has range 2–4096. ARM64_4K_PAGES is a page-size choice. HZ_100 selects
  the HZ=100 default, and HIGH_RES_TIMERS selects hidden TICK_ONESHOT under
  GENERIC_CLOCKEVENTS. PREEMPT_NONE selects PREEMPT_NONE_BUILD.
- PL011 depends on ARM_AMBA and the enclosing TTY menu; its console requires
  PL011=y and selects SERIAL_CORE_CONSOLE and SERIAL_EARLYCON. SERIAL_CORE is
  selected by PL011. UNIX is under NET. BINFMT_ELF and SHMEM require MMU;
  TMPFS requires SHMEM, and DEVTMPFS_MOUNT requires DEVTMPFS.
- ARM_PMU is under PERF_EVENTS; EVTSTREAM depends on ARM_ARCH_TIMER;
  STACKPROTECTOR_STRONG depends on STACKPROTECTOR; SME depends on SVE and BROKEN;
  MTE depends on assembler capabilities, TAGGED_ADDR_ABI and PAN. These are off.
- Remaining debug, tracing, security, power, firmware, virtualization, CPU-feature
  and tick choices were checked at the locations below and resolved to the
  requested disabled state. Compiler-dependent choices remain subject to the
  same value check when built with Ubuntu GCC.

| Symbol (CONFIG_ prefix omitted) | Kconfig definition | Requested | Resolved |
| --- | --- | --- | --- |
| 64BIT | `arch/arm64/Kconfig:298` | `y` | `y` |
| ARM64 | `arch/arm64/Kconfig:2` | `y` | `y` |
| SMP | `arch/arm64/Kconfig:373` | `y` | `y` |
| NR_CPUS | `arch/arm64/Kconfig:1551` | `2` | `2` |
| ARM64_4K_PAGES | `arch/arm64/Kconfig:1379` | `y` | `y` |
| PRINTK | `init/Kconfig:1627` | `y` | `y` |
| BUG | `init/Kconfig:1638` | `y` | `y` |
| MULTIUSER | `init/Kconfig:1563` | `y` | `y` |
| FUTEX | `init/Kconfig:1672` | `y` | `y` |
| EPOLL | `init/Kconfig:1687` | `y` | `y` |
| POSIX_TIMERS | `init/Kconfig:1610` | `y` | `y` |
| HIGH_RES_TIMERS | `kernel/time/Kconfig:191` | `y` | `y` |
| TICK_ONESHOT | `kernel/time/Kconfig:91` | `y` | `y` |
| HZ_100 | `kernel/Kconfig.hz:20` | `y` | `y` |
| HZ | `kernel/Kconfig.hz:51` | `100` | `100` |
| BINFMT_ELF | `fs/Kconfig.binfmt:5` | `y` | `y` |
| BLK_DEV_INITRD | `init/Kconfig:1382` | `y` | `y` |
| DEVTMPFS | `drivers/base/Kconfig:31` | `y` | `y` |
| DEVTMPFS_MOUNT | `drivers/base/Kconfig:50` | `y` | `y` |
| TMPFS | `fs/Kconfig:175` | `y` | `y` |
| SHMEM | `init/Kconfig:1721` | `y` | `y` |
| PROC_FS | `fs/proc/Kconfig:2` | `y` | `y` |
| SYSFS | `fs/sysfs/Kconfig:2` | `y` | `y` |
| TTY | `drivers/tty/Kconfig:2` | `y` | `y` |
| SERIAL_AMBA_PL011 | `drivers/tty/serial/Kconfig:48` | `y` | `y` |
| SERIAL_AMBA_PL011_CONSOLE | `drivers/tty/serial/Kconfig:59` | `y` | `y` |
| SERIAL_EARLYCON | `drivers/tty/serial/Kconfig:9` | `y` | `y` |
| UNIX | `net/unix/Kconfig:6` | `y` | `y` |
| NET | `net/Kconfig:6` | `y` | `y` |
| OF | `drivers/of/Kconfig:5` | `y` | `y` |
| ARM_GIC_V3 | `drivers/irqchip/Kconfig:35` | `y` | `y` |
| ARM_PSCI_FW | `drivers/firmware/psci/Kconfig:2` | `y` | `y` |
| COMMON_CLK | `drivers/clk/Kconfig:21` | `y` | `y` |
| LOCALVERSION | `init/Kconfig:228` | `"-seedbox-arm64-pv1"` | `"-seedbox-arm64-pv1"` |
| LOCALVERSION_AUTO | `init/Kconfig:238` | `n` | `n` |
| BUILD_SALT | `init/Kconfig:259` | `""` | `""` |
| ARM_ARCH_TIMER | `drivers/clocksource/Kconfig:314` | `n` | `n (absent)` |
| ARM_ARCH_TIMER_EVTSTREAM | `drivers/clocksource/Kconfig:319` | `n` | `n (absent)` |
| ARM_PMU | `drivers/perf/Kconfig:58` | `n` | `n (absent)` |
| PERF_EVENTS | `init/Kconfig:1902` | `n` | `n` |
| KVM | `arch/arm64/kvm/Kconfig:20` | `n` | `n (absent)` |
| PARAVIRT | `arch/arm64/Kconfig:1603` | `n` | `n` |
| ACPI | `drivers/acpi/Kconfig:9` | `n` | `n (absent)` |
| EFI | `arch/arm64/Kconfig:2440` | `n` | `n` |
| PCI | `drivers/pci/Kconfig:21` | `n` | `n` |
| HW_RANDOM | `drivers/char/hw_random/Kconfig:6` | `n` | `n` |
| RANDOMIZE_BASE | `arch/arm64/Kconfig:2331` | `n` | `n` |
| RANDOMIZE_KSTACK_OFFSET_DEFAULT | `arch/Kconfig:1429` | `n` | `n` |
| STACKPROTECTOR | `arch/Kconfig:646` | `n` | `n (absent)` |
| STACKPROTECTOR_STRONG | `arch/Kconfig:670` | `n` | `n (absent)` |
| ARM64_PTR_AUTH | `arch/arm64/Kconfig:2006` | `n` | `n` |
| ARM64_BTI | `arch/arm64/Kconfig:2116` | `n` | `n` |
| ARM64_SVE | `arch/arm64/Kconfig:2250` | `n` | `n` |
| ARM64_SME | `arch/arm64/Kconfig:2281` | `n` | `n (absent)` |
| ARM64_MTE | `arch/arm64/Kconfig:2179` | `n` | `n (absent)` |
| ARM64_PSEUDO_NMI | `arch/arm64/Kconfig:2293` | `n` | `n` |
| CPU_IDLE | `drivers/cpuidle/Kconfig:4` | `n` | `n` |
| CPU_FREQ | `drivers/cpufreq/Kconfig:4` | `n` | `n` |
| PM | `kernel/power/Kconfig:205` | `n` | `n` |
| SUSPEND | `kernel/power/Kconfig:2` | `n` | `n` |
| COMPAT | `arch/arm64/Kconfig:1750` | `n` | `n` |
| MODULES | `kernel/module/Kconfig:2` | `n` | `n` |
| DEBUG_INFO | `lib/Kconfig.debug:227` | `n` | `n (absent)` |
| FTRACE | `kernel/trace/Kconfig:182` | `n` | `n` |
| KASAN | `lib/Kconfig.kasan:34` | `n` | `n (absent)` |
| KCSAN | `lib/Kconfig.kcsan:13` | `n` | `n (absent)` |
| UBSAN | `lib/Kconfig.ubsan:5` | `n` | `n` |
| SECURITY | `security/Kconfig:54` | `n` | `n` |
| AUDIT | `init/Kconfig:495` | `n` | `n` |
| NO_HZ_IDLE | `kernel/time/Kconfig:108` | `n` | `n` |
| NO_HZ_FULL | `kernel/time/Kconfig:118` | `n` | `n` |
| PREEMPT_NONE | `kernel/Kconfig.preempt:18` | `y` | `y` |

Additional source/build review:

- `apply.py --check` and application both succeeded against pristine pinned source.
  The CPU-feature register switch had one remaining direct CNTFRQ_EL0 read;
  it now uses the patched fixed-frequency helper, avoiding a later counter-audit
  failure. The vmlinux/vDSO instruction audit remains mandatory.
- Tool preflight now includes the archive decompressor, Perl, filesystem probe
  and binutils used by Kbuild. Static musl init compilation runs before kernel
  extraction/compilation so compiler/libc problems fail early.
- `make prepare` on Darwin stopped at `scripts/sorttable.c`: missing `<elf.h>`.
  No full Image build, GNU-objdump audit, musl init build, or hardware boot is
  claimed here. Those still require the documented Ubuntu arm64 VM dependencies.
  The Clang configuration pass is not a substitute for the GCC build.
- The downloaded archive and extracted tree were removed after validation.
