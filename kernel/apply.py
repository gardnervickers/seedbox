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
shutil.copy(here / "seedbox_clock.h", src / "arch/x86/include/asm/seedbox_clock.h")
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

# User clock reads without a trap. The vDSO's TSC read uses the clock fields
# of the time page, mapped writable and shared into every process as one more
# page at the top of the vvar area. Raw user RDTSC still traps (CR4.TSD).
edit(
    "arch/x86/entry/vdso/vdso-layout.lds.S",
    "\tvvar_start = . - 4 * PAGE_SIZE;\n",
    "\tvvar_start = . - 5 * PAGE_SIZE;\n",
)
edit(
    "arch/x86/entry/vdso/vdso-layout.lds.S",
    "\ttimens_page  = vvar_start + 3 * PAGE_SIZE;\n",
    "\ttimens_page  = vvar_start + 3 * PAGE_SIZE;\n\tseedbox_clock_page = vvar_start + 4 * PAGE_SIZE;\n",
)
edit(
    "arch/x86/include/asm/vdso/gettimeofday.h",
    "#include <clocksource/hyperv_timer.h>\n",
    "#include <clocksource/hyperv_timer.h>\n#include <asm/seedbox_clock.h>\n\n"
    "/* Last page of the vvar area: the time page's clock fields, writable. */\n"
    "extern struct seedbox_clock seedbox_clock_page\n\t__attribute__((visibility(\"hidden\")));\n",
)
edit(
    "arch/x86/include/asm/vdso/gettimeofday.h",
    "\t\treturn (u64)rdtsc_ordered() & S64_MAX;\n",
    "\t\treturn seedbox_clock_read(&seedbox_clock_page) & S64_MAX;\n",
)
edit(
    "arch/x86/entry/vdso/vma.c",
    "static const struct vm_special_mapping vvar_mapping = {\n\t.name = \"[vvar]\",\n\t.fault = vvar_fault,\n};\n",
    "static const struct vm_special_mapping vvar_mapping = {\n\t.name = \"[vvar]\",\n\t.fault = vvar_fault,\n};\n\n"
    "static vm_fault_t seedbox_clock_fault(const struct vm_special_mapping *sm,\n"
    "\t\t\t\t       struct vm_area_struct *vma, struct vm_fault *vmf)\n{\n"
    "\treturn vmf_insert_pfn(vma, vmf->address, SEEDBOX_TIME_PAGE_PHYS >> PAGE_SHIFT);\n}\n\n"
    "static const struct vm_special_mapping seedbox_clock_mapping = {\n"
    "\t.name = \"[seedbox_clock]\",\n\t.fault = seedbox_clock_fault,\n};\n",
)
# The vvar mapping keeps its four pages; the clock page above it is its own
# shared, writable mapping.
edit(
    "arch/x86/entry/vdso/vma.c",
    "\tvma = _install_special_mapping(mm,\n\t\t\t\t       addr,\n\t\t\t\t       -image->sym_vvar_start,\n",
    "\tvma = _install_special_mapping(mm,\n\t\t\t\t       addr,\n\t\t\t\t       -image->sym_vvar_start - PAGE_SIZE,\n",
)
edit(
    "arch/x86/entry/vdso/vma.c",
    "\tif (IS_ERR(vma)) {\n\t\tret = PTR_ERR(vma);\n\t\tdo_munmap(mm, text_start, image->size, NULL);\n\t} else {\n",
    "\tif (IS_ERR(vma)) {\n\t\tret = PTR_ERR(vma);\n\t\tdo_munmap(mm, text_start, image->size, NULL);\n\t\tgoto up_fail;\n\t}\n\n"
    "\tvma = _install_special_mapping(mm,\n\t\t\t\t       text_start - PAGE_SIZE,\n\t\t\t\t       PAGE_SIZE,\n"
    "\t\t\t\t       VM_READ|VM_WRITE|VM_SHARED|VM_MAYREAD|\n"
    "\t\t\t\t       VM_MAYWRITE|VM_MAYSHARE|VM_IO|VM_DONTDUMP|\n"
    "\t\t\t\t       VM_PFNMAP,\n\t\t\t\t       &seedbox_clock_mapping);\n\n"
    "\tif (IS_ERR(vma)) {\n\t\tret = PTR_ERR(vma);\n"
    "\t\tdo_munmap(mm, addr, -image->sym_vvar_start - PAGE_SIZE, NULL);\n"
    "\t\tdo_munmap(mm, text_start, image->size, NULL);\n\t} else {\n",
)
edit(
    "arch/x86/entry/vdso/vma.c",
    "\t\t\t\tvma_is_special_mapping(vma, &vvar_mapping)) {\n",
    "\t\t\t\tvma_is_special_mapping(vma, &vvar_mapping) ||\n"
    "\t\t\t\tvma_is_special_mapping(vma, &seedbox_clock_mapping)) {\n",
)
edit("arch/x86/entry/vdso/vma.c", "#include <clocksource/hyperv_timer.h>\n", "#include <clocksource/hyperv_timer.h>\n#include <asm/seedbox.h>\n")

# CR4.TSD must stay set for every task.
edit(
    "arch/x86/kernel/process.c",
    "int set_tsc_mode(unsigned int val)\n{\n",
    "int set_tsc_mode(unsigned int val)\n{\n\treturn -EINVAL;\n",
)
print("seedbox guest changes applied")
