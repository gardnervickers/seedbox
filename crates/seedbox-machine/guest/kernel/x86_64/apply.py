#!/usr/bin/env python3
"""Apply narrowly checked x86-64 PV ABI 3 edits to pristine Linux 6.12.112.

This is an experimental dedicated guest kernel, not a general Linux patch. It
fails on source drift and on reapplication.
"""
import argparse
from pathlib import Path

HERE = Path(__file__).resolve().parent


def replace(text, old, new, count=1):
    if text.count(old) != count:
        raise ValueError(f"expected {count} occurrences of {old!r}, found {text.count(old)}")
    return text.replace(old, new)


def changes(root):
    edits = {}

    def edit(path, fn):
        edits[path] = fn(edits.get(path, None) or (root / path).read_text())

    edit("arch/x86/kernel/Makefile", lambda s: replace(
        s, "obj-y\t\t\t+= ebda.o\n", "obj-y\t\t\t+= ebda.o\nobj-y\t\t\t+= seedbox.o\n"))

    # Kernel relaxation loops make bounded PV progress even without a syscall
    # or clock read. Keep vDSO, setup and decompressor PAUSE instructions raw.
    guard = "#if !defined(BUILD_VDSO) && !defined(__DISABLE_EXPORTS) && !defined(_SETUP)\n"
    edit("arch/x86/include/asm/vdso/processor.h", lambda s: replace(replace(
        s,
        "/* REP NOP (PAUSE) is a good thing to insert into busy-wait loops. */",
        guard + "extern void seedbox_cpu_relax(void);\n#endif\n\n"
        "/* REP NOP (PAUSE) is a good thing to insert into busy-wait loops. */"),
        '\tasm volatile("rep; nop" ::: "memory");\n',
        '\tasm volatile("rep; nop" ::: "memory");\n' + guard + "\tseedbox_cpu_relax();\n#endif\n"))

    # Kernel TSC reads return PV time. The vDSO and boot decompressor keep the
    # raw instruction; application mode traps through CR4.TSD.
    def msr(s):
        s = replace(
            s,
            "static __always_inline unsigned long long rdtsc(void)\n{",
            "#if !defined(BUILD_VDSO) && !defined(__DISABLE_EXPORTS)\n"
            "extern unsigned long long seedbox_rdtsc(void);\n"
            "static __always_inline unsigned long long rdtsc(void)\n{\n\treturn seedbox_rdtsc();\n}\n"
            "static __always_inline unsigned long long rdtsc_ordered(void)\n{\n\treturn seedbox_rdtsc();\n}\n"
            "#else\n"
            "static __always_inline unsigned long long rdtsc(void)\n{")
        return replace(
            s,
            "\t\t\t:: \"ecx\");\n\n\treturn EAX_EDX_VAL(val, low, high);\n}\n",
            "\t\t\t:: \"ecx\");\n\n\treturn EAX_EDX_VAL(val, low, high);\n}\n#endif\n")
    edit("arch/x86/include/asm/msr.h", msr)

    # Emulate application RDTSC/RDTSCP ahead of the UMIP fixup.
    def traps(s):
        s = replace(s, "#include <asm/traps.h>\n", "#include <asm/traps.h>\n#include <asm/seedbox.h>\n")
        return replace(
            s,
            "\tcond_local_irq_enable(regs);\n\n\tif (static_cpu_has(X86_FEATURE_UMIP)) {\n",
            "\tcond_local_irq_enable(regs);\n\n"
            "\tif (user_mode(regs) && seedbox_emulate_user_tsc(regs))\n\t\tgoto exit;\n\n"
            "\tif (static_cpu_has(X86_FEATURE_UMIP)) {\n")
    edit("arch/x86/kernel/traps.c", traps)

    # Every 64-bit syscall charges the published PV price.
    def entry(s):
        s = replace(s, "#include <asm/traps.h>\n", "#include <asm/traps.h>\n#include <asm/seedbox.h>\n")
        return replace(
            s,
            "\tinstrumentation_begin();\n\n\tif (!do_syscall_x64(regs, nr)",
            "\tinstrumentation_begin();\n\tseedbox_syscall_progress();\n\n\tif (!do_syscall_x64(regs, nr)")
    edit("arch/x86/entry/common.c", entry)

    # Fixed 1 GHz TSC and the PV timer instead of HPET/PIT calibration and ticks.
    def x86_init(s):
        s = replace(s, "#include <asm/iommu.h>\n", "#include <asm/iommu.h>\n#include <asm/seedbox.h>\n")
        s = replace(s, "\t\t.timer_init\t\t= hpet_time_init,\n", "\t\t.timer_init\t\t= seedbox_timer_init,\n")
        return replace(
            s,
            "\t.calibrate_cpu\t\t\t= native_calibrate_cpu_early,\n\t.calibrate_tsc\t\t\t= native_calibrate_tsc,\n",
            "\t.calibrate_cpu\t\t\t= seedbox_tsc_khz,\n\t.calibrate_tsc\t\t\t= seedbox_tsc_khz,\n")
    edit("arch/x86/kernel/x86_init.c", x86_init)

    # The emulated PIC mask read has no side effects or hardware timing role.
    # Its unused read during IRQ acknowledgment only adds a VM exit.
    edit("arch/x86/kernel/i8259.c", lambda s: replace(
        s, "\t\tinb(PIC_MASTER_IMR);\t/* DUMMY - (do we need this?) */\n", ""))

    edit("arch/x86/include/asm/fixmap.h", lambda s: replace(
        s, "\tFIX_EARLYCON_MEM_BASE,\n",
        "\tFIX_EARLYCON_MEM_BASE,\n\tFIX_SEEDBOX_TIME,\n\tFIX_SEEDBOX_EVENT,\n"))

    def setup(s):
        s = replace(s, "#include <asm/proto.h>\n", "#include <asm/proto.h>\n#include <asm/seedbox.h>\n")
        return replace(s, "\ttsc_early_init();\n", "\tseedbox_time_init();\n\ttsc_early_init();\n")
    edit("arch/x86/kernel/setup.c", setup)

    # The PV platform has no ACPI reset register, keyboard controller, EFI or
    # BIOS. The default fallback chain would spin on absent ports and finally
    # execute empty real-mode memory without any exit. A triple fault is the
    # platform's only architectural reset; the worker reports it as a crash.
    edit("kernel/reboot.c", lambda s: replace(
        s, "enum reboot_type reboot_type = BOOT_ACPI;\n",
        "/* Seedbox x86 PV: triple fault is the only platform reset. */\n"
        "enum reboot_type reboot_type = BOOT_TRIPLE;\n"))

    # CR4.TSD must stay set for every task: PR_SET_TSC cannot re-enable host time.
    edit("arch/x86/kernel/process.c", lambda s: replace(
        s, "int set_tsc_mode(unsigned int val)\n{\n",
        "int set_tsc_mode(unsigned int val)\n{\n\treturn -EINVAL;\n"))

    # Initial SSE/YMM state needs one saved representation in guest RAM.
    # SSE's MXCSR rules depend on the XSAVE format; standard saves stay intact.
    edit("arch/x86/kernel/fpu/xstate.h", lambda s: replace(replace(
        s,
        "static inline void os_xsave(struct fpstate *fpstate)",
        "void seedbox_canonicalize_task_xstate(struct fpstate *fpstate);\n\n"
        "static inline void os_xsave(struct fpstate *fpstate)"),
        "\tXSTATE_XSAVE(&fpstate->regs.xsave, lmask, hmask, err);\n\n"
        "\t/* We should never fault when copying to a kernel buffer: */\n"
        "\tWARN_ON_FPU(err);\n",
        "\tXSTATE_XSAVE(&fpstate->regs.xsave, lmask, hmask, err);\n\n"
        "\t/* We should never fault when copying to a kernel buffer: */\n"
        "\tWARN_ON_FPU(err);\n"
        "\tif (!err)\n\t\tseedbox_canonicalize_task_xstate(fpstate);\n"))
    anchor = "\treturn (void *)xsave + xfeature_get_offset(xcomp_bv, xfeature_nr);\n}\n"
    edit("arch/x86/kernel/fpu/xstate.c", lambda s: replace(s, anchor, anchor + """
/* Seedbox: XINUSE may describe initial state as either present or absent.
 * Canonicalize the saved representation without changing register values.
 * Compacted XRSTOR(S) initializes MXCSR when SSE is absent; standard XRSTOR
 * loads MXCSR independently, so leave standard-format SSE state untouched.
 * This task-save hook does not cover signal frames or application XSAVE.
 */
void seedbox_canonicalize_task_xstate(struct fpstate *fpstate)
{
	struct xregs_state *xsave = &fpstate->regs.xsave;
	struct fxregs_state *sse = &xsave->i387;
	void *ymm;

	if ((fpstate->xfeatures & XFEATURE_MASK_SSE) &&
	    (xsave->header.xcomp_bv & XCOMP_BV_COMPACTED_FORMAT)) {
		if (!(xsave->header.xfeatures & XFEATURE_MASK_SSE)) {
			memset(sse->xmm_space, 0, sizeof(sse->xmm_space));
			sse->mxcsr = MXCSR_DEFAULT;
		} else if (sse->mxcsr == MXCSR_DEFAULT &&
			   !memchr_inv(sse->xmm_space, 0, sizeof(sse->xmm_space))) {
			xsave->header.xfeatures &= ~XFEATURE_MASK_SSE;
		}
	}

	if (!(fpstate->xfeatures & XFEATURE_MASK_YMM))
		return;
	ymm = __raw_xsave_addr(xsave, XFEATURE_YMM);
	if (!ymm)
		return;
	if (!(xsave->header.xfeatures & XFEATURE_MASK_YMM))
		memset(ymm, 0, xstate_sizes[XFEATURE_YMM]);
	else if (!memchr_inv(ymm, 0, xstate_sizes[XFEATURE_YMM]))
		xsave->header.xfeatures &= ~XFEATURE_MASK_YMM;
}
"""))

    for name, target in [("seedbox.c", "arch/x86/kernel/seedbox.c"),
                         ("seedbox.h", "arch/x86/include/asm/seedbox.h")]:
        if (root / target).exists():
            raise ValueError(f"already patched: {target}")
        edits[target] = (HERE / name).read_text()
    return edits


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--check", action="store_true", help="check anchors without writing")
    args = parser.parse_args()
    edits = changes(args.source)
    for path, content in edits.items():
        if not args.check:
            (args.source / path).write_text(content)
        print(path)


if __name__ == "__main__":
    main()
