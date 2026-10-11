"""Exercise the actual kernel read functions against the same scheduling bounds."""
from pathlib import Path
import os
import shlex
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class ClockPolicy(unittest.TestCase):
    def test_both_kernels_charge_100ns_without_returning_past_a_bound(self):
        common = r'''
#include <assert.h>
#include <stdint.h>
#include <stdlib.h>
typedef uint64_t u64;
#define notrace
#define noinstr
#define likely(x) (x)
#define unlikely(x) (x)
#define READ_ONCE(x) (x)
#define WRITE_ONCE(x, v) ((x) = (v))
#define panic(...) abort()
static unsigned long irq_flags;
#define local_irq_save(f) do { (f) = irq_flags; irq_flags |= 128; } while (0)
#define local_irq_restore(f) (irq_flags = (f))
#define local_irq_enable() (irq_flags = 0)
#define local_irq_disable() (irq_flags = 128)
#define irqs_disabled_flags(f) ((f) & 128)
#define isb() ((void)0)
static unsigned yields;
static void advance(void);
'''
        for arch in ['x86', 'arm64']:
            with self.subTest(arch=arch):
                if arch == 'arm64':
                    source = (ROOT / 'kernel-arm64/seedbox.c').read_text()
                    functions = source[source.index('static u64 notrace seedbox_read_step'):
                                       source.index('/* splitmix64')]
                    setup = r'''
static struct { u64 now, last, bound, abi, deferred_irq_pending; } state, *page = &state;
static u64 early_last;
static char registers[64];
static void *event = registers;
static void writeq(u64 value, void *address) {
    assert(value == 1 && address == registers + 24); advance();
}
#define read_clock seedbox_read
#define state_now state.now
'''
                else:
                    source = (ROOT / 'kernel/seedbox.c').read_text()
                    functions = source[source.index('noinstr unsigned long long seedbox_rdtsc'):
                                       source.index('bool seedbox_emulate_user_tsc')]
                    setup = r'''
struct seedbox_time_page { u64 tsc, last, bound; };
static struct seedbox_time_page state, *seedbox_page = &state;
static unsigned long long seedbox_early_last;
#define SEEDBOX_PORT_YIELD 1
#define SEEDBOX_PORT_TIME_LO 2
#define SEEDBOX_PORT_TIME_HI 3
static void outl(unsigned value, unsigned port) {
    assert(value == 1 && port == 1); advance();
}
static unsigned inl(unsigned port) { (void)port; abort(); }
#define read_clock seedbox_rdtsc
#define state_now state.tsc
'''
                    constant = next(line for line in (ROOT / 'kernel/seedbox.h').read_text().splitlines()
                                    if line.startswith('#define SEEDBOX_PV_READ_NS'))
                    setup = constant + '\n' + setup
                main = r'''
static void advance(void) {
    ++yields;
    state_now = state.bound;
    state.bound += 250;
}
int main(void) {
    for (int masked = 0; masked < 2; ++masked) {
        irq_flags = masked ? 128 : 0;
        state_now = state.last = 0; state.bound = 250; yields = 0;
        for (u64 expected = 100; expected <= 10000; expected += 100) {
            assert(read_clock() == expected);
            assert(state.last <= state.bound);
            assert(irq_flags == (masked ? 128UL : 0UL));
        }
        assert(yields == 39);
        /* A publication ahead of the reader is observed directly. */
        state_now = 20000; state.bound = 20100;
        assert(read_clock() == 20000);
        assert(read_clock() == 20100);  /* The exact bound is legal. */
        assert(read_clock() == 20200 && yields == 40);
    }
}
'''
                with tempfile.TemporaryDirectory() as directory:
                    c = Path(directory) / 'clock.c'
                    exe = Path(directory) / 'clock'
                    c.write_text(common + setup + functions + main)
                    subprocess.run(shlex.split(os.environ.get('CC', 'cc')) +
                                   ['-Wall', '-Wextra', '-Werror', str(c), '-o', str(exe)],
                                   check=True, capture_output=True)
                    subprocess.run([str(exe)], check=True, capture_output=True, timeout=10)
