
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
