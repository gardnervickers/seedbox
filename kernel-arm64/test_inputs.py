#!/usr/bin/env python3
"""Portable tests; no kernel compiler or hypervisor needed."""
import importlib.util
from pathlib import Path
import stat
import os
import shlex
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent

def load(name, file):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

apply = load("apply", "apply.py")
initramfs = load("initramfs", "build-initramfs.py")
audit = load("audit", "audit.py")
config = load("config", "check-config.py")

class Inputs(unittest.TestCase):
    def test_config_rejects_unknown_disabled_and_dropped_requests(self):
        requested = config.settings('CONFIG_PRESENT=y\nCONFIG_REMOVED=n\nCONFIG_HZ=100\n')
        actual = config.settings('# CONFIG_PRESENT is not set\nCONFIG_HZ=1000\n')
        self.assertEqual(config.errors(requested, actual, {'CONFIG_PRESENT', 'CONFIG_HZ'}), [
            'CONFIG_PRESENT: requested y, resolved n',
            'CONFIG_REMOVED: not defined in arm64 Kconfig sources',
            'CONFIG_HZ: requested 100, resolved 1000',
        ])
        self.assertEqual(config.errors(config.settings('# CONFIG_PRESENT is not set\n'),
                                       {}, {'CONFIG_PRESENT'}), [])
        with self.assertRaises(ValueError):
            config.settings('CONFIG_PRESENT=y\nCONFIG_PRESENT=n\n')

    def test_config_definitions_exclude_other_architectures(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for file, text in {
                'arch/Kconfig': 'config GENERIC\n',
                'arch/arm64/Kconfig': '\tconfig ARM64_ONLY\n',
                'arch/x86/Kconfig': 'config X86_ONLY\n',
                'drivers/Kconfig': 'menuconfig DRIVER\n',
            }.items():
                path = root / file
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
            self.assertEqual(config.defined_symbols(root), {
                'CONFIG_GENERIC', 'CONFIG_ARM64_ONLY', 'CONFIG_DRIVER',
            })

    def test_newc_independent_parse(self):
        image = initramfs.archive(b"ELF-test")
        self.assertEqual(image, initramfs.archive(b"ELF-test"))
        self.assertEqual(len(image) % 512, 0)
        entries, offset = {}, 0
        while True:
            self.assertEqual(image[offset:offset+6], b"070701")
            fields = [int(image[offset+6+i*8:offset+14+i*8], 16) for i in range(13)]
            mode, size, namesize = fields[1], fields[6], fields[11]
            offset += 110
            name = image[offset:offset+namesize-1].decode()
            offset = (offset + namesize + 3) & ~3
            entries[name] = (mode, image[offset:offset+size], fields[9:11])
            offset = (offset + size + 3) & ~3
            if name == "TRAILER!!!": break
        self.assertEqual(entries["init"], (stat.S_IFREG | 0o755, b"ELF-test", [0, 0]))
        self.assertEqual(entries["dev/console"], (stat.S_IFCHR | 0o600, b"", [5, 1]))

    def test_patch_refuses_drift(self):
        self.assertEqual(apply.body("int f(void) { if (a) { b(); } return 0; }", "f", "return 1;"), "int f(void) {\nreturn 1;\n}")
        with self.assertRaises(ValueError): apply.body("int f(void);", "f", "")
        with self.assertRaises(ValueError): apply.replace("xx", "x", "y")

    def test_idle_rendezvous_unmasks_and_restores_irq_state(self):
        # Compile the actual helper against a small IRQ/MMIO model. Check both
        # incoming mask states, not just the normal masked cpu_do_idle caller.
        source = (HERE / "seedbox.c").read_text()
        idle = source[source.index("void seedbox_idle(void)"):
                      source.index("void __init seedbox_time_init(void)")]
        fixture = r"""
#include <assert.h>
#include <stdlib.h>
static unsigned long irq_flags;
static int synchronized, writes;
static char registers[64];
static void *event = registers;
#define unlikely(x) (x)
#define panic(...) abort()
#define local_irq_save(saved) do { (saved) = irq_flags; irq_flags |= 128; } while (0)
#define local_irq_enable() do { irq_flags &= ~128UL; synchronized = 0; } while (0)
#define local_irq_restore(saved) do { irq_flags = (saved); } while (0)
#define dsb(scope) ((void)0)
#define isb() do { synchronized = 1; } while (0)
static void writeq(unsigned long value, void *address) {
    assert(value == 1 && address == registers + 16);
    assert(!(irq_flags & 128) && synchronized);
    ++writes;
}
""" + idle + r"""
int main(void) {
    irq_flags = 0x3c0;
    seedbox_idle();
    assert(irq_flags == 0x3c0 && writes == 1);
    irq_flags = 0x340;
    seedbox_idle();
    assert(irq_flags == 0x340 && writes == 2);
}
"""
        with tempfile.TemporaryDirectory() as directory:
            c = Path(directory) / "idle.c"
            exe = Path(directory) / "idle"
            c.write_text(fixture)
            subprocess.run(shlex.split(os.environ.get("CC", "cc")) + [
                "-Wall", "-Wextra", "-Werror", str(c), "-o", str(exe)
            ], check=True, capture_output=True)
            subprocess.run([str(exe)], check=True, capture_output=True)

    def test_exec_cannot_reenable_hardware_counter(self):
        source = """static void update_cntkctl_el1(struct task_struct *next)
{
\tif (test_ti_thread_flag(task_thread_info(next), TIF_TSC_SIGSEGV))
\t\tsysreg_clear_set(cntkctl_el1, ARCH_TIMER_USR_VCT_ACCESS_EN, 0);
\telse
\t\tsysreg_clear_set(cntkctl_el1, 0, ARCH_TIMER_USR_VCT_ACCESS_EN);
}
"""
        patched = apply.counter_access(source)
        self.assertNotIn("cntkctl_el1, 0, ARCH_TIMER_USR_VCT_ACCESS_EN", patched)
        self.assertEqual(patched.count("cntkctl_el1, ARCH_TIMER_USR_VCT_ACCESS_EN, 0"), 2)
        self.assertIn("TIF_TSC_SIGSEGV", patched)
        with self.assertRaises(ValueError): apply.counter_access(patched)

    def test_counter_audit_rejects_real_and_encoded_leaks(self):
        for instruction in ["mrs x0, cntvct_el0", "mrs x4, cntpctss_el0", "mrs x0, s3_3_c14_c0_6", "mrs x0, RNDR", "wfit x3", "mrs x0, CNTFRQ_EL0"]:
            self.assertTrue(audit.violations(instruction), instruction)
        self.assertFalse(audit.violations("msr cntv_ctl_el0, xzr\nmrs x0, cntkctl_el1\n<arch_timer_read_cntvct_el0>"))

if __name__ == "__main__": unittest.main()
