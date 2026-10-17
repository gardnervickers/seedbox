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
    def test_hrtimer_spin_rejects_low_resolution_mode(self):
        # Exercise the actual guest prerequisite with Linux's boot-time (one
        # jiffy) and active high-resolution (1ns) clock_getres results.
        source = (HERE / "init.c").read_text()
        helper = source[source.index("static void require_high_resolution(void)"):
                        source.index("static void busy_wait_check(void)")]
        fixture = r"""
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static long resolution_ns;
static int getres(clockid_t id, struct timespec *res) {
    assert(id == CLOCK_MONOTONIC);
    if (resolution_ns < 0) { errno = EIO; return -1; }
    res->tv_sec = 0;
    res->tv_nsec = resolution_ns;
    return 0;
}
static void fail(const char *what) {
    fprintf(stderr, "%s: errno=%d\n", what, errno);
    exit(2);
}
#define clock_getres getres
""" + helper + r"""
int main(int argc, char **argv) {
    assert(argc == 2);
    resolution_ns = strtol(argv[1], NULL, 10);
    require_high_resolution();
    return 0;
}
"""
        with tempfile.TemporaryDirectory() as directory:
            c = Path(directory) / "resolution.c"
            exe = Path(directory) / "resolution"
            c.write_text(fixture)
            subprocess.run(shlex.split(os.environ.get("CC", "cc")) + [
                "-Wall", "-Wextra", "-Werror", str(c), "-o", str(exe)
            ], check=True, capture_output=True)
            for resolution, status, diagnostic in [
                (1, 0, ""),
                (10000000, 2, "PV high-resolution timer mode not active"),
                (0, 2, "PV high-resolution timer mode not active"),
                (-1, 2, "clock_getres"),
            ]:
                result = subprocess.run([str(exe), str(resolution)],
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertIn(diagnostic, result.stderr)

    def test_fixture_requires_hrtimers_and_timerfd_even_if_fragment_omits_them(self):
        required = {'CONFIG_HIGH_RES_TIMERS': 'y', 'CONFIG_TIMERFD': 'y'}
        self.assertEqual(config.fixture_errors(required), [])
        for key in required:
            for value in [None, 'n', 'm']:
                actual = required.copy()
                if value is None:
                    del actual[key]
                else:
                    actual[key] = value
                self.assertEqual(config.fixture_errors(actual), [
                    f'{key}: timerfd spin fixture requires y'])
        self.assertEqual(len(config.fixture_errors({})), 2)

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

    def test_kernel_poll_hook_is_active_only_outside_vdso(self):
        header = 'static inline void cpu_relax(void)\n{\n\tasm volatile("yield" ::: "memory");\n}\n'
        patched = apply.polling(header)
        # The host may be x86. Replace only the architecture hint; compile the
        # actual patched call and guard, then check the positive/negative cases.
        patched = patched.replace('asm volatile("yield" ::: "memory")', 'asm volatile("" ::: "memory")')
        fixture = '#include <assert.h>\nstatic unsigned reads;\n' + patched + '\n' + r"""
void seedbox_cpu_relax(void) { ++reads; }
int main(void) {
    for (unsigned i = 0; i < 123; ++i) cpu_relax();
#ifdef BUILD_VDSO
    assert(reads == 0);
#else
    assert(reads == 123);
#endif
}
"""
        with tempfile.TemporaryDirectory() as directory:
            c = Path(directory) / "relax.c"
            exe = Path(directory) / "relax"
            c.write_text(fixture)
            for flags in [[], ["-DBUILD_VDSO"]]:
                subprocess.run(shlex.split(os.environ.get("CC", "cc")) + flags + [
                    "-Wall", "-Wextra", "-Werror", str(c), "-o", str(exe)
                ], check=True, capture_output=True)
                subprocess.run([str(exe)], check=True, capture_output=True)

    def test_poll_yield_preserves_caller_mask_and_retries_after_irq(self):
        source = (HERE / "seedbox.c").read_text()
        read = source[source.index("static u64 notrace seedbox_read_step(u64 step)"):
                      source.index("/* splitmix64")]
        fixture = r"""
#include <assert.h>
#include <stdint.h>
#include <stdlib.h>
typedef uint64_t u64;
#define notrace
#define unlikely(x) (x)
#define panic(...) abort()
static unsigned long irq_flags, caller_flags;
static int synchronized, writes;
static char registers[64];
static void *event = registers;
static struct { u64 now, last, bound, abi, deferred_irq_pending; } storage, *page = &storage;
static u64 early_last;
#define local_irq_save(saved) do { (saved) = irq_flags; irq_flags |= 128; } while (0)
#define local_irq_enable() do { irq_flags &= ~128UL; synchronized = 0; } while (0)
#define local_irq_disable() do { irq_flags |= 128; } while (0)
#define local_irq_restore(saved) do { irq_flags = (saved); } while (0)
#define irqs_disabled_flags(saved) ((saved) & 128)
#define isb() do { synchronized = 1; } while (0)
#define READ_ONCE(x) (assert(irq_flags & 128), (x))
#define WRITE_ONCE(x, v) do { assert(irq_flags & 128); (x) = (v); } while (0)
static void writeq(unsigned long value, void *address) {
    assert(value == 1 && address == registers + 24);
    assert(irq_flags == caller_flags);
    if (!(caller_flags & 128)) assert(synchronized);
    ++writes;
    page->now = 100;
    page->bound = UINT64_MAX;
    page->deferred_irq_pending = !!(caller_flags & 128);
    /* Simulate IRQ clock reads during the unmasked rendezvous. */
    if (!(caller_flags & 128)) page->last = 105;
}
""" + read + r"""
int main(void) {
    for (int masked = 0; masked != 2; ++masked) {
        caller_flags = irq_flags = masked ? 0x3c0 : 0x340;
        storage.now = storage.last = storage.bound = 100;
        storage.deferred_irq_pending = 0;
        writes = synchronized = 0;
        assert(seedbox_read() == (masked ? 200 : 205));
        assert(irq_flags == caller_flags && writes == 1);
        assert(seedbox_read() == (masked ? 300 : 305));
        assert(irq_flags == caller_flags && writes == 1);
        assert(storage.deferred_irq_pending == (u64)masked);
        if (masked) {
            /* No idle or other MMIO: the next unmasked read delivers it. */
            caller_flags = irq_flags = 0x340;
            assert(seedbox_read() == 205);
            assert(irq_flags == caller_flags && writes == 2);
            assert(storage.deferred_irq_pending == 0);
            assert(seedbox_read() == 305 && writes == 2);
        }
    }
}
"""
        with tempfile.TemporaryDirectory() as directory:
            c = Path(directory) / "poll.c"
            exe = Path(directory) / "poll"
            c.write_text(fixture)
            subprocess.run(shlex.split(os.environ.get("CC", "cc")) + [
                "-Wall", "-Wextra", "-Werror", str(c), "-o", str(exe)
            ], check=True, capture_output=True)
            subprocess.run([str(exe)], check=True, capture_output=True, timeout=10)

    def run_charge_fixture(self, main):
        # The actual charge helpers against a PV page and rendezvous model.
        # Each rendezvous takes a tick in hard IRQ context on top of
        # `nesting`, and counts how tick accounting classifies it.
        source = (HERE / "seedbox.c").read_text()
        page = source[source.index("struct seedbox_page {"):
                      source.index("static struct seedbox_page *page;")]
        helpers = source[source.index("static u64 notrace seedbox_read_step(u64 step)"):
                         source.index("static u64 cs_read")]
        fixture = r"""
#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
typedef uint64_t u64;
#define notrace
#define unlikely(x) (x)
#define panic(...) abort()
typedef uint8_t u8;
struct pt_regs { unsigned long regs[31]; };
struct task_struct { int pid; };
static struct task_struct tasks[2], *current = &tasks[0];
#define HARDIRQ_OFFSET (1UL << 16)
#define SOFTIRQ_OFFSET (1UL << 8)
static unsigned long preempt, nesting;
#define irq_count() preempt
static unsigned user_ticks, system_ticks;
bool seedbox_user_tick(void);
static struct task_struct *user_charge; /* tentative; defined in the helpers */
""" + page + r"""
_Static_assert(offsetof(struct seedbox_page, syscall_ns) == 48, "PV syscall cost ABI");
_Static_assert(offsetof(struct seedbox_page, progress_ns) == 56, "progress charge ABI");
_Static_assert(offsetof(struct seedbox_page, progress_jitter_ns) == 64, "progress jitter ABI");
_Static_assert(offsetof(struct seedbox_page, progress_jitter_seed) == 72, "progress jitter ABI");
_Static_assert(offsetof(struct seedbox_page, preempt_ppm) == 120, "scheduler perturbation ABI");
_Static_assert(offsetof(struct seedbox_page, preempt_seed) == 128, "scheduler perturbation ABI");
static struct seedbox_page storage, *page = &storage;
static struct pt_regs regs;
static u64 early_last, next_cost, window;
static unsigned long irq_flags;
static unsigned int writes, yields;
static void yield(void) { assert(irq_flags == 0); ++yields; }
static char registers[64];
static void *event = registers;
#define local_irq_save(saved) do { (saved) = irq_flags; irq_flags |= 128; } while (0)
#define local_irq_enable() do { irq_flags &= ~128UL; } while (0)
#define local_irq_disable() do { irq_flags |= 128; } while (0)
#define local_irq_restore(saved) do { irq_flags = (saved); } while (0)
#define irqs_disabled_flags(saved) ((saved) & 128)
#define isb() ((void)0)
#define AT(x, y) ((void *)&(x) == (void *)&(y))
#define READ_ONCE(x) (assert(AT(x, page->syscall_ns) || AT(x, page->progress_ns) \
                             || AT(x, page->progress_jitter_ns) \
                             || AT(x, page->progress_jitter_seed) \
                             || AT(x, page->preempt_ppm) || AT(x, page->preempt_seed) \
                             || AT(x, user_charge) || (irq_flags & 128)), (x))
#define likely(x) (x)
#define WRITE_ONCE(x, v) do { assert(AT(x, user_charge) || (irq_flags & 128)); \
                              (x) = (v); } while (0)
static void writeq(unsigned long value, void *address) {
    assert(value == 1 && address == registers + 24);
    assert(irq_flags == 0);
    assert(page->last == page->bound);
    ++writes;
    page->now = page->last;
    page->bound += window;
    if (next_cost) page->syscall_ns = next_cost;
    preempt = nesting + HARDIRQ_OFFSET;
    if (seedbox_user_tick()) ++user_ticks; else ++system_ticks;
    preempt = 0;
}
""" + helpers + main
        with tempfile.TemporaryDirectory() as directory:
            c = Path(directory) / "syscall.c"
            exe = Path(directory) / "syscall"
            c.write_text(fixture)
            subprocess.run(shlex.split(os.environ.get("CC", "cc")) + [
                "-Wall", "-Wextra", "-Werror", "-O2", str(c), "-o", str(exe)
            ], check=True, capture_output=True)
            subprocess.run([str(exe)], check=True, capture_output=True, timeout=10)

    def test_syscall_charge_scales_and_survives_mid_charge_throttle_and_heal(self):
        self.run_charge_fixture(r"""
static void reset(u64 cost, u64 bound, u64 span, u64 replacement) {
    storage = (struct seedbox_page){.syscall_ns = cost, .bound = bound};
    window = span;
    next_cost = replacement;
    writes = 0;
}
int main(void) {
    /* Ordinary clock reads cost 100 ns, independently of CPU throttle. */
    for (u64 cost = 1000; cost <= 4000; cost *= 4) {
        reset(cost, UINT64_MAX, 0, 0);
        assert(seedbox_read() == 100);
        seedbox_syscall_progress(&regs, 0);
        assert(page->last == cost + 100 && writes == 0);
        assert(irq_flags == 0);
    }
    reset(1000000, UINT64_MAX, 0, 0);
    seedbox_syscall_progress(&regs, 0);
    assert(page->last == 1000000 && writes == 0);

    /* Throttling halfway through affects the next syscall, not owed work. */
    reset(1000, 500, 500, 4000);
    seedbox_syscall_progress(&regs, 0);
    assert(page->last == 1000 && writes == 1 && page->syscall_ns == 4000);
    seedbox_syscall_progress(&regs, 0);
    assert(page->last == 5000 && writes == 9);

    /* Healing halfway through likewise preserves the original charge. */
    reset(4000, 2000, 2000, 1000);
    seedbox_syscall_progress(&regs, 0);
    assert(page->last == 4000 && writes == 1 && page->syscall_ns == 1000);
    seedbox_syscall_progress(&regs, 0);
    assert(page->last == 5000 && writes == 2);

    /* Multiple bounded rendezvous neither lose nor duplicate work. */
    reset(1000, 250, 250, 0);
    seedbox_syscall_progress(&regs, 0);
    assert(page->last == 1000 && writes == 3 && irq_flags == 0);

    /* A marked progress call charges the published nominal cost instead of
     * one syscall; without one it charges a syscall. Other syscalls and a
     * plain getpid charge a syscall. */
    reset(1000, UINT64_MAX, 0, 0);
    regs.regs[0] = SEEDBOX_PROGRESS_MARK;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(page->last == 1000);
    page->progress_ns = 25000;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(page->last == 26000 && writes == 0);
    seedbox_syscall_progress(&regs, 1);
    regs.regs[0] = 0;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(page->last == 28000);

    /* Time syscalls charge nothing here; their clock read costs 100 ns. */
    reset(1000, UINT64_MAX, 0, 0);
    seedbox_syscall_progress(&regs, SEEDBOX_CLOCK_GETTIME_NR);
    seedbox_syscall_progress(&regs, SEEDBOX_CLOCK_GETRES_NR);
    seedbox_syscall_progress(&regs, SEEDBOX_GETTIMEOFDAY_NR);
    assert(page->last == 0 && writes == 0);
    seedbox_syscall_progress(&regs, 0);
    assert(page->last == 1000);
    /* Translator syscalls cost a tenth of a syscall, scaled by throttle. */
    regs.regs[7] = SEEDBOX_TRANSLATOR_MARK;
    seedbox_syscall_progress(&regs, 98);
    assert(page->last == 1100);
    page->syscall_ns = 4000;
    seedbox_syscall_progress(&regs, 98);
    assert(page->last == 1500);
    page->syscall_ns = 1000;
    regs.regs[7] = 0;
    page->last = 28000;
    page->progress_ns = 25000;

    /* A throttle scales the compute charge like a syscall's. */
    page->syscall_ns = 4000;
    regs.regs[0] = SEEDBOX_PROGRESS_MARK;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(page->last == 128000 && writes == 0);

    /* A bulk charge rendezvous at every bound like single reads would. */
    reset(1000, 10000, 10000, 0);
    page->progress_ns = 25000;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(page->last == 25000 && writes == 2 && irq_flags == 0);

    /* With a jitter bound each call draws within the bound either way; a zero
     * bound charges every call the same, a changed seed restarts the stream,
     * and the bound is capped at the nominal charge. */
    reset(1000, UINT64_MAX, 0, 0);
    page->progress_ns = 25000;
    page->progress_jitter_ns = 5000;
    page->progress_jitter_seed = 7;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    u64 first = page->last;
    assert(first >= 20000 && first <= 30000 && writes == 0);
    page->progress_jitter_seed = 8;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    u64 other = page->last - first;
    assert(other >= 20000 && other <= 30000);
    page->progress_jitter_seed = 7;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(page->last == first + other + first);
    page->progress_jitter_ns = 0;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(page->last == first + other + first + 25000 && irq_flags == 0);
    page->progress_jitter_ns = UINT64_MAX;
    for (int i = 0; i != 64; ++i) {
        u64 before = page->last;
        seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
        assert(page->last - before <= 50000);
    }

    /* Scheduler perturbation yields at its rate on its own seeded stream: never
     * when off, at every call at a million per million, and a changed seed
     * restarts the stream. It charges no time. */
    reset(1000, UINT64_MAX, 0, 0);
    yields = 0;
    for (int i = 0; i != 100; ++i) seedbox_syscall_progress(&regs, 1);
    assert(yields == 0 && page->last == 100000);
    page->preempt_ppm = 1000000;
    page->preempt_seed = 3;
    for (int i = 0; i != 100; ++i) seedbox_syscall_progress(&regs, 1);
    assert(yields == 100 && page->last == 200000);
    page->preempt_ppm = 250000;
    page->preempt_seed = 11;
    yields = 0;
    for (int i = 0; i != 1000; ++i) seedbox_syscall_progress(&regs, 1);
    unsigned int quarter = yields;
    assert(quarter > 150 && quarter < 350);
    page->preempt_seed = 12;
    yields = 0;
    for (int i = 0; i != 1000; ++i) seedbox_syscall_progress(&regs, 1);
    page->preempt_seed = 11;
    yields = 0;
    for (int i = 0; i != 1000; ++i) seedbox_syscall_progress(&regs, 1);
    assert(yields == quarter);
    page->preempt_ppm = 0;

    /* Preserve the pre-initialization syscall behavior. */
    page = NULL;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(early_last == 1000 && irq_flags == 0);
    regs.regs[0] = 0;
    seedbox_syscall_progress(&regs, 1);
    assert(early_last == 2000 && irq_flags == 0);
}
""")

    def test_ticks_during_progress_charges_count_as_user_time(self):
        self.run_charge_fixture(r"""
static void reset(u64 bound, u64 span) {
    storage = (struct seedbox_page){.syscall_ns = 1000, .bound = bound,
                                    .progress_ns = 25000};
    window = span;
    next_cost = 0;
    writes = user_ticks = system_ticks = 0;
    nesting = 0;
    current = &tasks[0];
}
int main(void) {
    /* A tick during a progress charge samples user work, at every bound
     * the charge crosses; the charge total is unchanged. */
    reset(10000, 10000);
    regs.regs[0] = SEEDBOX_PROGRESS_MARK;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(page->last == 25000 && writes == 2);
    assert(user_ticks == 2 && system_ticks == 0);
    /* After the call returns, a tick is the kernel's again. */
    preempt = HARDIRQ_OFFSET;
    assert(!seedbox_user_tick());
    preempt = 0;

    /* Syscall and translator work stays system time. */
    reset(500, 500);
    regs.regs[0] = 0;
    seedbox_syscall_progress(&regs, 1);
    regs.regs[7] = SEEDBOX_TRANSLATOR_MARK;
    for (int i = 0; i != 10; ++i)
        seedbox_syscall_progress(&regs, 98);
    regs.regs[7] = 0;
    regs.regs[0] = SEEDBOX_PROGRESS_MARK;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR + 1);
    assert(page->last == 3000 && writes == 5);
    assert(user_ticks == 0 && system_ticks == 5);

    /* A tick nested in a softirq on top of the charge, or seen by another
     * task, is not the charge's. */
    reset(10000, 10000);
    nesting = SOFTIRQ_OFFSET;
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(user_ticks == 0 && system_ticks == 2);
    nesting = 0;
    reset(UINT64_MAX, 0);
    current = &tasks[1];
    assert(!seedbox_user_tick());

    /* Before the PV page, a marked call charges a syscall, as system time. */
    page = NULL;
    current = &tasks[0];
    seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
    assert(early_last == 1000);
    preempt = HARDIRQ_OFFSET;
    assert(!seedbox_user_tick());
}
""")

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
