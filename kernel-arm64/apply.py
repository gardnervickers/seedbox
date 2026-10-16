#!/usr/bin/env python3
"""Apply narrowly checked edits to pristine Linux 6.12.112; fail on drift/reapply.

This is an experimental dedicated guest kernel, not a general Linux patch.
"""
import argparse
from pathlib import Path
import re

HERE = Path(__file__).resolve().parent

def replace(text, old, new, count=1):
    if text.count(old) != count:
        raise ValueError(f"expected {count} occurrences of {old!r}, found {text.count(old)}")
    return text.replace(old, new)

def body(text, name, replacement):
    matches = list(re.finditer(r"\b" + re.escape(name) + r"\([^;{}]*\)\s*\{", text))
    if len(matches) != 1:
        raise ValueError(f"expected one definition of {name}, found {len(matches)}")
    start = matches[0].end()
    depth, end = 1, start
    while depth:
        if text[end] == "{": depth += 1
        if text[end] == "}": depth -= 1
        end += 1
    return text[:start] + "\n" + replacement + "\n" + text[end-1:]

def counter_access(text):
    # Exec calls do_set_tsc_mode(PR_TSC_ENABLE); context switches and prctl
    # also reach this helper. Preserve the task's SIGSEGV policy in traps.c,
    # but never let PR_TSC_ENABLE bypass the PV trap handler to host time.
    return replace(text,
        "\t\tsysreg_clear_set(cntkctl_el1, 0, ARCH_TIMER_USR_VCT_ACCESS_EN);",
        "\t\t/* Seedbox: PR_TSC_ENABLE means PV emulation, not hardware access. */\n"
        "\t\tsysreg_clear_set(cntkctl_el1, ARCH_TIMER_USR_VCT_ACCESS_EN, 0);")

def futex_retries(text):
    # With one guest CPU and PREEMPT_NONE, exclusive-store failures cannot
    # represent contention from another guest task. Retry host-induced failures
    # locally instead of exposing -EAGAIN and its cond_resched/PV clock path.
    text = replace(text,
        "#define FUTEX_MAX_LOOPS\t128 /* What's the largest number you can think of? */\n\n",
        "/* Seedbox retries host-induced exclusive failures on its single guest CPU. */\n\n")
    text = replace(text,
        "\tunsigned int loops = FUTEX_MAX_LOOPS;\t\t\t\t\\\n"
        "\t\t\t\t\t\t\t\t\t\\\n", "")
    text = replace(text,
        '"\tcbz\t%w0, 3f\\n"\t\t\t\t\t\t\\\n'
        '"\tsub\t%w4, %w4, %w0\\n"\t\t\t\t\t\\\n'
        '"\tcbnz\t%w4, 1b\\n"\t\t\t\t\t\t\\\n'
        '"\tmov\t%w0, %w6\\n"\t\t\t\t\t\t\\\n',
        '"\tcbnz\t%w0, 1b\\n"\t\t\t\t\t\t\\\n')
    text = replace(text,
        '\t: "=&r" (ret), "=&r" (oldval), "+Q" (*uaddr), "=&r" (tmp),\t\\\n'
        '\t  "+r" (loops)\t\t\t\t\t\t\t\\\n'
        '\t: "r" (oparg), "Ir" (-EAGAIN)\t\t\t\t\t\\\n',
        '\t: "=&r" (ret), "=&r" (oldval), "+Q" (*uaddr), "=&r" (tmp)\t\\\n'
        '\t: "r" (oparg)\t\t\t\t\t\t\t\\\n')
    for instruction in ["mov\t%w3, %w5", "add\t%w3, %w1, %w5",
                        "orr\t%w3, %w1, %w5", "and\t%w3, %w1, %w5",
                        "eor\t%w3, %w1, %w5"]:
        text = replace(text, instruction, instruction.replace("%w5", "%w4"))
    text = replace(text, "\tunsigned int loops = FUTEX_MAX_LOOPS;\n", "")
    text = replace(text, '"\tsub\t%w3, %w1, %w5\\n"',
                   '"\tsub\t%w3, %w1, %w4\\n"')
    text = replace(text,
        '"2:\tstlxr\t%w3, %w6, %2\\n"\n'
        '"\tcbz\t%w3, 3f\\n"\n'
        '"\tsub\t%w4, %w4, %w3\\n"\n'
        '"\tcbnz\t%w4, 1b\\n"\n'
        '"\tmov\t%w0, %w7\\n"\n',
        '"2:\tstlxr\t%w3, %w5, %2\\n"\n'
        '"\tcbnz\t%w3, 1b\\n"\n')
    return replace(text,
        '\t: "+r" (ret), "=&r" (val), "+Q" (*uaddr), "=&r" (tmp), "+r" (loops)\n'
        '\t: "r" (oldval), "r" (newval), "Ir" (-EAGAIN)\n',
        '\t: "+r" (ret), "=&r" (val), "+Q" (*uaddr), "=&r" (tmp)\n'
        '\t: "r" (oldval), "r" (newval)\n')

def polling(text):
    # Kernel C polling only: do not charge raw exclusive retry assembly, whose
    # retries may be caused by host interruptions rather than guest work.
    text = replace(text, "static inline void cpu_relax(void)",
                   "#ifndef BUILD_VDSO\nvoid seedbox_cpu_relax(void);\n#endif\n\nstatic inline void cpu_relax(void)")
    return replace(text, '\tasm volatile("yield" ::: "memory");',
                   '\tasm volatile("yield" ::: "memory");\n#ifndef BUILD_VDSO\n\tseedbox_cpu_relax();\n#endif')

def changes(root, application=False):
    edits = {}
    def edit(path, fn):
        edits[path] = fn((root / path).read_text())
    edit("arch/arm64/Kconfig", lambda s: replace(s, "\tselect ARM_ARCH_TIMER\n", "\t# Seedbox uses its dedicated PV clocksource.\n"))
    edit("arch/arm64/kernel/Makefile", lambda s: s + "\n# Dedicated deterministic guest input.\nobj-y += seedbox.o\n")
    def time(s):
        s = replace(s, "#include <asm/paravirt.h>", "#include <asm/paravirt.h>\n#include <asm/seedbox.h>")
        return body(s, "time_init", "\tof_clk_init(NULL);\n\tseedbox_time_init();\n\ttick_setup_hrtimer_broadcast();\n\tlpj_fine = 1000000000UL / HZ;")
    edit("arch/arm64/kernel/time.c", time)
    edit("arch/arm64/kernel/process.c", counter_access)
    edit("arch/arm64/include/asm/futex.h", futex_retries)
    edit("arch/arm64/include/asm/vdso/processor.h", polling)
    # Every return to EL0 crosses a guest exception/scheduling boundary, unlike
    # a host-only interruption. Publish its epoch and clear the physical
    # reservation before restoring user x0/x1; EL1 returns do not publish one.
    # Keep the restore instructions in the anchor so source drift fails closed.
    edit("arch/arm64/kernel/entry.S", lambda s: replace(s,
        "\tmsr\telr_el1, x21\t\t\t// set up the return data\n"
        "\tmsr\tspsr_el1, x22\n"
        "\tldp\tx0, x1, [sp, #16 * 0]\n",
        "\t.if \\el == 0\n"
        "\tadrp\tx0, seedbox_exclusive_epoch\n"
        "\tadd\tx0, x0, :lo12:seedbox_exclusive_epoch\n"
        "\tldr\tx0, [x0]\n"
        "\tldr\tx1, [x0]\n"
        "\tadd\tx1, x1, #1\n"
        "\tstr\tx1, [x0]\n"
        "\tmsr\ttpidrro_el0, x1\n"
        "\tclrex\n"
        "\t.endif\n"
        "\tmsr\telr_el1, x21\t\t\t// set up the return data\n"
        "\tmsr\tspsr_el1, x22\n"
        "\tldp\tx0, x1, [sp, #16 * 0]\n"))
    def syscall(s):
        s = replace(s, "#include <asm/syscall.h>",
                    "#include <asm/syscall.h>\n#include <asm/seedbox.h>")
        return replace(s, "\tadd_random_kstack_offset();",
                       "\tseedbox_syscall_progress(regs, scno);\n\tadd_random_kstack_offset();")
    edit("arch/arm64/kernel/syscall.c", syscall)

    def generic_timer(s):
        # The driver is off; still route all generic helpers to PV (traps etc.).
        start = s.index("#else", s.index("#ifdef CONFIG_ARM_ARCH_TIMER"))
        s = s[:start] + replace(s[start:], "#else\n", "#else\n#include <asm/seedbox.h>\n", 1)
        s = body(s, "arch_timer_get_rate", "\treturn 1000000000U;")
        return body(s, "arch_timer_read_counter", "\treturn seedbox_read();")
    edit("include/clocksource/arm_arch_timer.h", generic_timer)
    def arch_timer(s):
        s = replace(s, "#include <asm/barrier.h>", "#include <asm/barrier.h>\n#include <asm/seedbox.h>")
        for name in ["arch_timer_read_cntpct_el0", "arch_timer_read_cntvct_el0", "__arch_counter_get_cntpct_stable", "__arch_counter_get_cntpct", "__arch_counter_get_cntvct_stable", "__arch_counter_get_cntvct"]:
            s = body(s, name, "\treturn seedbox_read();")
        return body(s, "arch_timer_get_cntfrq", "\treturn 1000000000U;")
    edit("arch/arm64/include/asm/arch_timer.h", arch_timer)
    # The generic CPU feature-register switch also reads CNTFRQ directly.
    # Keep it consistent with cpuinfo and the PV timer, including in dead code
    # that the mandatory vmlinux instruction audit still inspects.
    def cpufeature(s):
        s = replace(s, "#include <asm/cpu.h>",
                    "#include <asm/arch_timer.h>\n#include <asm/cpu.h>")
        return replace(s, "\tread_sysreg_case(SYS_CNTFRQ_EL0);",
                       "\tcase SYS_CNTFRQ_EL0:\n\t\treturn arch_timer_get_cntfrq();")
    edit("arch/arm64/kernel/cpufeature.c", cpufeature)
    def delay(s):
        s = body(s, "__delay_cycles", "\treturn seedbox_read();")
        return body(s, "__delay", "\tcycles_t start = __delay_cycles();\n\twhile ((__delay_cycles() - start) < cycles)\n\t\tcpu_relax();")
    edit("arch/arm64/lib/delay.c", delay)
    edit("arch/arm64/kernel/idle.c", lambda s: replace(replace(s, "#include <asm/sysreg.h>", "#include <asm/sysreg.h>\n#include <asm/seedbox.h>"), "\twfi();", "\tseedbox_idle();"))
    def traps(s):
        # CNTVCT/CNTVCTSS already use arch_timer_read_counter(), now PV.
        # CNTPCT/CNTPCTSS need explicit handlers as well (op2 1 / 5).
        old = "static const struct sys64_hook sys64_hooks[] = {"
        new = old + """
	{
		.esr_mask = ESR_ELx_SYS64_ISS_SYS_OP_MASK,
		.esr_val = ESR_ELx_SYS64_ISS_SYS_CNTVCT ^ (3UL << 17),
		.handler = cntvct_read_handler,
	},
	{
		.esr_mask = ESR_ELx_SYS64_ISS_SYS_OP_MASK,
		.esr_val = ESR_ELx_SYS64_ISS_SYS_CNTVCTSS ^ (3UL << 17),
		.handler = cntvct_read_handler,
	},"""
        return replace(s, old, new)
    edit("arch/arm64/kernel/traps.c", traps)
    # Force all vDSO clock requests through syscalls, including coarse clocks;
    # remove even unreachable counter instructions from the vDSO image.
    edit("arch/arm64/include/asm/vdso/gettimeofday.h", lambda s: body(s, "__arch_get_hw_counter", "\treturn 0; /* seedbox: only syscall clock reads */"))
    def vdso(s):
        s = replace(s, "__cvdso_clock_gettime(clock, ts)", "clock_gettime_fallback(clock, ts)")
        s = replace(s, "__cvdso_gettimeofday(tv, tz)", "gettimeofday_fallback(tv, tz)")
        return replace(s, "__cvdso_clock_getres(clock_id, res)", "clock_getres_fallback(clock_id, res)")
    edit("arch/arm64/kernel/vdso/vgettimeofday.c", vdso)
    def rng(s):
        for name in ["smccc_probe_trng", "__arm64_rndr", "__arm64_rndrrs", "__cpu_has_rng", "__early_cpu_has_rndr"]:
            s = body(s, name, "\treturn false; /* seedbox: entropy comes from seeded FDT */")
        for name in ["arch_get_random_longs", "arch_get_random_seed_longs"]:
            s = body(s, name, "\treturn 0; /* no host RNDR or firmware RNG */")
        return s
    edit("arch/arm64/include/asm/archrandom.h", rng)
    # Branch entropy (as kernel/apply.py on x86). A reseed input at a
    # simulated time reaches a stopped guest without its cooperation: the VMM
    # writes entropy into the PV page and bumps a generation. The next random
    # output, batched draw or tick mixes it into the input pool and reseeds.
    def random(s):
        s = replace(s, "#include <linux/random.h>\n",
                    "#include <linux/random.h>\n#include <asm/seedbox.h>\n")
        s = replace(s,
            "static void crng_make_state(u32 chacha_state[CHACHA_STATE_WORDS],\n",
            "static void __crng_make_state(u32 chacha_state[CHACHA_STATE_WORDS],\n"
            "\t\t\t      u8 *random_data, size_t random_data_len);\n\n"
            "static void crng_make_state(u32 chacha_state[CHACHA_STATE_WORDS],\n"
            "\t\t\t    u8 *random_data, size_t random_data_len)\n{\n"
            "\tseedbox_take_entropy();\n"
            "\t__crng_make_state(chacha_state, random_data, random_data_len);\n}\n\n"
            "static void __crng_make_state(u32 chacha_state[CHACHA_STATE_WORDS],\n")
        s, n = re.subn(r"\n(\tnext_gen = READ_ONCE\(base_crng\.generation\);[ \t]*\\\n)",
                       lambda m: "\n\tseedbox_take_entropy();\t\t\t\t\t\t\t\\\n" + m.group(1), s)
        if n != 1:
            raise ValueError(f"expected one batched generation check, found {n}")
        return s + (
            "\nvoid seedbox_take_entropy(void)\n{\n"
            "\tstatic u64 seen;\n\tunsigned long flags;\n\tu8 entropy[32];\n\tu64 gen;\n\n"
            "\tif (likely(seedbox_entropy_gen() == seen))\n\t\treturn;\n"
            "\t/* No interrupt may draw from the old key mid-reseed. */\n"
            "\tlocal_irq_save(flags);\n"
            "\tgen = seedbox_entropy_gen();\n"
            "\tif (gen != seen) {\n"
            "\t\tseedbox_read_entropy(entropy);\n"
            "\t\tmix_pool_bytes(entropy, sizeof(entropy));\n"
            "\t\tmemzero_explicit(entropy, sizeof(entropy));\n"
            "\t\t/* Before the CRNG is ready, mixing is enough. */\n"
            "\t\tif (crng_ready())\n\t\t\tcrng_reseed(NULL);\n"
            "\t\tseen = gen;\n"
            "\t}\n"
            "\tlocal_irq_restore(flags);\n}\n")
    edit("drivers/char/random.c", random)
    for name in ["seedbox.c", "seedbox.h"]:
        target = "arch/arm64/" + ("kernel/" if name.endswith(".c") else "include/asm/") + name
        if (root / target).exists(): raise ValueError(f"already patched: {target}")
        edits[target] = (HERE / name).read_text()
    if not application:
        edits["arch/arm64/kernel/seedbox.c"] += (HERE / "poll-test.c").read_text()
    if application:
        edit("drivers/char/Kconfig", lambda s: s + "\nconfig SEEDBOX_SUPERVISOR_IO\n\tbool \"Seedbox supervisor register device\"\n\tdepends on ARM64 && OF\n")
        edit("drivers/char/Makefile", lambda s: s + "\nobj-$(CONFIG_SEEDBOX_SUPERVISOR_IO) += supervisor-io.o\n")
        for name in ["supervisor-io.c", "supervisor-io.h"]:
            target = "drivers/char/" + name
            if (root / target).exists(): raise ValueError(f"already patched: {target}")
            edits[target] = (HERE / name).read_text()
    return edits

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("source", type=Path)
    p.add_argument("--check", action="store_true", help="check anchors without writing")
    p.add_argument("--application", action="store_true", help="include the dedicated Compose supervisor device")
    args = p.parse_args()
    edits = changes(args.source, args.application)
    for path, content in edits.items():
        if not args.check: (args.source / path).write_text(content)
        print(path)
if __name__ == "__main__": main()
