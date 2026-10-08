#!/usr/bin/env python3
"""Apply the Seedbox guest changes to a clean Linux 6.12.112 tree (idempotent check)."""
import pathlib
import re
import shutil
import sys

src = pathlib.Path(sys.argv[1])
here = pathlib.Path(__file__).resolve().parent


def edit(rel, old, new, count=1):
    path = src / rel
    text = path.read_text()
    if text.count(old) != count:
        raise SystemExit(f"{rel}: expected {count} match(es) of patch anchor")
    path.write_text(text.replace(old, new))


def edit_re(rel, pattern, repl, count=1):
    path = src / rel
    text, n = re.subn(pattern, repl, path.read_text())
    if n != count:
        raise SystemExit(f"{rel}: expected {count} match(es) of patch pattern, found {n}")
    path.write_text(text)


def append(rel, text):
    path = src / rel
    path.write_text(path.read_text() + text)


shutil.copy(here / "seedbox.c", src / "arch/x86/kernel/seedbox.c")
shutil.copy(here / "seedbox.h", src / "arch/x86/include/asm/seedbox.h")
edit("arch/x86/kernel/Makefile", "obj-y\t\t\t+= ebda.o\n", "obj-y\t\t\t+= ebda.o\nobj-y\t\t\t+= seedbox.o\n")

# Kernel TSC reads return VMM-published virtual time. The vDSO and the boot
# decompressor keep the raw instruction (user mode traps via CR4.TSD).
edit(
    "arch/x86/include/asm/msr.h",
    "static __always_inline unsigned long long rdtsc(void)\n{",
    "#if !defined(BUILD_VDSO) && !defined(__DISABLE_EXPORTS)\n"
    "extern unsigned long long seedbox_rdtsc(void);\n"
    "static __always_inline unsigned long long rdtsc(void)\n{\n\treturn seedbox_rdtsc();\n}\n"
    "static __always_inline unsigned long long rdtsc_ordered(void)\n{\n\treturn seedbox_rdtsc();\n}\n"
    "#else\n"
    "static __always_inline unsigned long long rdtsc(void)\n{",
)
edit(
    "arch/x86/include/asm/msr.h",
    "\t\t\t:: \"ecx\");\n\n\treturn EAX_EDX_VAL(val, low, high);\n}\n",
    "\t\t\t:: \"ecx\");\n\n\treturn EAX_EDX_VAL(val, low, high);\n}\n#endif\n",
)

edit(
    "arch/x86/kernel/traps.c",
    "\tif (user_mode(regs)) {\n\t\tif (fixup_iopl_exception(regs))\n",
    "\tif (user_mode(regs)) {\n\t\tif (seedbox_emulate_user_tsc(regs))\n\t\t\tgoto exit;\n\n"
    "\t\tif (fixup_iopl_exception(regs))\n",
)
edit("arch/x86/kernel/traps.c", "#include <asm/traps.h>\n", "#include <asm/traps.h>\n#include <asm/seedbox.h>\n")

edit(
    "arch/x86/entry/common.c",
    "\tinstrumentation_begin();\n\n\tif (!do_syscall_x64(regs, nr)",
    "\tinstrumentation_begin();\n\tseedbox_syscall_hook();\n\n\tif (!do_syscall_x64(regs, nr)",
)
edit("arch/x86/entry/common.c", "#include <asm/traps.h>\n", "#include <asm/traps.h>\n#include <asm/seedbox.h>\n")

# Fixed 1 GHz TSC: skip PIT-based TSC and CPU calibration at boot.
edit(
    "arch/x86/kernel/x86_init.c",
    "\t.calibrate_cpu\t\t\t= native_calibrate_cpu_early,\n\t.calibrate_tsc\t\t\t= native_calibrate_tsc,\n",
    "\t.calibrate_cpu\t\t\t= seedbox_tsc_khz,\n\t.calibrate_tsc\t\t\t= seedbox_tsc_khz,\n",
)
edit("arch/x86/kernel/x86_init.c", "#include <asm/iommu.h>\n", "#include <asm/iommu.h>\n#include <asm/seedbox.h>\n")

# Activate the time page before TSC-based delays and sched_clock start using
# it. A permanent fixmap works before memremap/the allocator are available;
# unlike an early_memremap slot, it needs no later unmap/remap handoff.
edit(
    "arch/x86/include/asm/fixmap.h",
    "\tFIX_EARLYCON_MEM_BASE,\n",
    "\tFIX_EARLYCON_MEM_BASE,\n\tFIX_SEEDBOX_TIME,\n",
)
edit("arch/x86/kernel/setup.c", "#include <asm/proto.h>\n", "#include <asm/proto.h>\n#include <asm/seedbox.h>\n")
edit(
    "arch/x86/kernel/setup.c",
    "\ttsc_early_init();\n",
    "\tseedbox_time_init();\n\ttsc_early_init();\n",
)

# Interrupt masking and acknowledgment through the shared page. The VMM's
# 8259 keeps no in-service state, so masking a line is its acknowledgment.
pic_ports = (
    "\tif (irq & 8)\n\t\toutb(cached_slave_mask, PIC_SLAVE_IMR);\n"
    "\telse\n\t\toutb(cached_master_mask, PIC_MASTER_IMR);\n"
)
edit(
    "arch/x86/kernel/i8259.c",
    pic_ports,
    "\tif (seedbox_page)\n\t\tWRITE_ONCE(seedbox_page->pic_imr, cached_irq_mask);\n\telse " + pic_ports[1:],
    count=2,
)
edit(
    "arch/x86/kernel/i8259.c",
    "handle_real_irq:\n\tif (irq & 8) {\n",
    "handle_real_irq:\n\tif (seedbox_page) {\n\t\tWRITE_ONCE(seedbox_page->pic_imr, cached_irq_mask);\n\t} else if (irq & 8) {\n",
)
edit("arch/x86/kernel/i8259.c", "#include <asm/i8259.h>\n", "#include <asm/i8259.h>\n#include <asm/seedbox.h>\n")

# Arm the oneshot timer with one port write instead of two.
edit(
    "drivers/clocksource/i8253.c",
    "\traw_spin_lock(&i8253_lock);\n\toutb_p(delta & 0xff , PIT_CH0);\t/* LSB */\n\toutb_p(delta >> 8 , PIT_CH0);\t\t/* MSB */\n",
    "\traw_spin_lock(&i8253_lock);\n\tif (seedbox_page) {\n\t\toutl(delta, SEEDBOX_PORT_TIMER);\n\t} else {\n"
    "\t\toutb_p(delta & 0xff , PIT_CH0);\t/* LSB */\n\t\toutb_p(delta >> 8 , PIT_CH0);\t/* MSB */\n\t}\n",
)
edit("drivers/clocksource/i8253.c", "#include <linux/i8253.h>\n", "#include <linux/i8253.h>\n#include <asm/seedbox.h>\n")

# Only the network device uses the shared-page ISR fast path. Other virtio
# devices have independent interrupt status registers.
edit(
    "drivers/virtio/virtio_mmio.c",
    "\tstatus = readl(vm_dev->base + VIRTIO_MMIO_INTERRUPT_STATUS);\n"
    "\twritel(status, vm_dev->base + VIRTIO_MMIO_INTERRUPT_ACK);\n",
    "\tif (seedbox_page && vm_dev->vdev.id.device == VIRTIO_ID_NET) {\n\t\tstatus = READ_ONCE(seedbox_page->net_isr);\n\t\tWRITE_ONCE(seedbox_page->net_isr, 0);\n\t} else {\n"
    "\t\tstatus = readl(vm_dev->base + VIRTIO_MMIO_INTERRUPT_STATUS);\n"
    "\t\twritel(status, vm_dev->base + VIRTIO_MMIO_INTERRUPT_ACK);\n\t}\n",
)
edit("drivers/virtio/virtio_mmio.c", "#include <linux/virtio_ring.h>\n", "#include <linux/virtio_ring.h>\n#include <linux/virtio_ids.h>\n#include <asm/seedbox.h>\n")

# CR4.TSD must stay set for every task.
edit(
    "arch/x86/kernel/process.c",
    "int set_tsc_mode(unsigned int val)\n{\n",
    "int set_tsc_mode(unsigned int val)\n{\n\treturn -EINVAL;\n",
)
# PV clock: crossings into user code pages. Executable VMAs map their pages
# NX, and the fault handler opens one page at a time (seedbox_exec_fault).
# Both checks sit behind the static key, so the counter timebase retires no
# extra branch.
edit(
    "arch/x86/mm/pgprot.c",
    "\tval = __sme_set(val);\n",
    "\tval = __sme_set(val);\n"
    "\tif (static_branch_unlikely(&seedbox_pv_clock) && (vm_flags & VM_EXEC))\n"
    "\t\tval |= _PAGE_NX;\n",
)
edit("arch/x86/mm/pgprot.c", "#include <asm/mem_encrypt.h>\n", "#include <asm/mem_encrypt.h>\n#include <asm/seedbox.h>\n")
edit(
    "arch/x86/mm/fault.c",
    "\tperf_sw_event(PERF_COUNT_SW_PAGE_FAULTS, 1, regs, address);\n",
    "\tperf_sw_event(PERF_COUNT_SW_PAGE_FAULTS, 1, regs, address);\n\n"
    "\tif (static_branch_unlikely(&seedbox_pv_clock) &&\n"
    "\t    seedbox_exec_fault(error_code, address))\n"
    "\t\treturn;\n",
)
edit("arch/x86/mm/fault.c", "#include <asm/fred.h>\n", "#include <asm/fred.h>\n#include <asm/seedbox.h>\n")

# Fork entropy (ABI 3). A world forked at a simulated time parks guests
# without their cooperation, so the VMM writes the seed's entropy into the time
# page and bumps a generation. The next random output mixes it into the input
# pool and reseeds, so each seed draws its own stream from the fork on. Batched
# get_random_u*() values check too, before they compare generations, and so
# does the timer interrupt: a released machine takes its late tick at once,
# and the reseed bumps the vDSO getrandom generation (glibc 2.41+, Go 1.24+),
# which never enters the kernel CRNG on its own.
edit(
    "drivers/char/random.c",
    "#include <linux/random.h>\n",
    "#include <linux/random.h>\n#include <asm/seedbox.h>\n",
)
edit(
    "drivers/char/random.c",
    "static void crng_make_state(u32 chacha_state[CHACHA_STATE_WORDS],\n",
    "static void __crng_make_state(u32 chacha_state[CHACHA_STATE_WORDS],\n"
    "\t\t\t      u8 *random_data, size_t random_data_len);\n\n"
    "static void crng_make_state(u32 chacha_state[CHACHA_STATE_WORDS],\n"
    "\t\t\t    u8 *random_data, size_t random_data_len)\n{\n"
    "\tseedbox_take_entropy();\n"
    "\t__crng_make_state(chacha_state, random_data, random_data_len);\n}\n\n"
    "static void __crng_make_state(u32 chacha_state[CHACHA_STATE_WORDS],\n",
)
edit_re(
    "drivers/char/random.c",
    r"\n(\tnext_gen = READ_ONCE\(base_crng\.generation\);[ \t]*\\\n)",
    lambda m: "\n\tseedbox_take_entropy();\t\t\t\t\t\t\t\\\n" + m.group(1),
)
edit(
    "arch/x86/kernel/time.c",
    "static irqreturn_t timer_interrupt(int irq, void *dev_id)\n{\n",
    "extern void seedbox_take_entropy(void);\n\n"
    "static irqreturn_t timer_interrupt(int irq, void *dev_id)\n{\n\tseedbox_take_entropy();\n",
)
append(
    "drivers/char/random.c",
    "\nvoid seedbox_take_entropy(void)\n{\n"
    "\tstruct seedbox_time_page *p = READ_ONCE(seedbox_page);\n"
    "\tstatic u64 seen;\n\tunsigned long flags;\n\tu64 gen;\n\n"
    "\tif (!p || likely(READ_ONCE(p->entropy_gen) == seen))\n\t\treturn;\n"
    "\t/* No interrupt may draw from the old key mid-reseed. */\n"
    "\tlocal_irq_save(flags);\n"
    "\tgen = READ_ONCE(p->entropy_gen);\n"
    "\tif (gen != seen) {\n"
    "\t\tmix_pool_bytes(p->entropy, sizeof(p->entropy));\n"
    "\t\t/* Before the CRNG is ready, mixing is enough: the boot path\n"
    "\t\t * seeds it, and reseeding here would mark it ready early. */\n"
    "\t\tif (crng_ready())\n\t\t\tcrng_reseed(NULL);\n"
    "\t\tseen = gen;\n"
    "\t}\n"
    "\tlocal_irq_restore(flags);\n}\n",
)
print("seedbox guest changes applied")
