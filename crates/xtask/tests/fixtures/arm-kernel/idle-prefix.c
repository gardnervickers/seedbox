
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
