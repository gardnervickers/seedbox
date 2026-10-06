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

# CR4.TSD must stay set for every task.
edit(
    "arch/x86/kernel/process.c",
    "int set_tsc_mode(unsigned int val)\n{\n",
    "int set_tsc_mode(unsigned int val)\n{\n\treturn -EINVAL;\n",
)
print("seedbox guest changes applied")
