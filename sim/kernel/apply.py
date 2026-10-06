#!/usr/bin/env python3
"""Apply the Seedbox guest changes to a clean Linux 6.12.112 tree (idempotent check)."""
import pathlib
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

# virtio-mmio interrupt status from the shared page instead of two MMIO exits.
edit(
    "drivers/virtio/virtio_mmio.c",
    "\tstatus = readl(vm_dev->base + VIRTIO_MMIO_INTERRUPT_STATUS);\n"
    "\twritel(status, vm_dev->base + VIRTIO_MMIO_INTERRUPT_ACK);\n",
    "\tif (seedbox_page) {\n\t\tstatus = READ_ONCE(seedbox_page->net_isr);\n\t\tWRITE_ONCE(seedbox_page->net_isr, 0);\n\t} else {\n"
    "\t\tstatus = readl(vm_dev->base + VIRTIO_MMIO_INTERRUPT_STATUS);\n"
    "\t\twritel(status, vm_dev->base + VIRTIO_MMIO_INTERRUPT_ACK);\n\t}\n",
)
edit("drivers/virtio/virtio_mmio.c", "#include <linux/virtio_ring.h>\n", "#include <linux/virtio_ring.h>\n#include <asm/seedbox.h>\n")

# CR4.TSD must stay set for every task.
edit(
    "arch/x86/kernel/process.c",
    "int set_tsc_mode(unsigned int val)\n{\n",
    "int set_tsc_mode(unsigned int val)\n{\n\treturn -EINVAL;\n",
)
print("seedbox guest changes applied")
