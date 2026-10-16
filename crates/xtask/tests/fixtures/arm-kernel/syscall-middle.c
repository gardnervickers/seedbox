
_Static_assert(offsetof(struct seedbox_page, syscall_ns) == 48, "PV syscall cost ABI");
static struct seedbox_page storage, *page = &storage;
static u64 early_last, next_cost, window;
static unsigned long irq_flags;
static unsigned int writes;
static char registers[64];
static void *event = registers;
#define local_irq_save(saved) do { (saved) = irq_flags; irq_flags |= 128; } while (0)
#define local_irq_enable() do { irq_flags &= ~128UL; } while (0)
#define local_irq_disable() do { irq_flags |= 128; } while (0)
#define local_irq_restore(saved) do { irq_flags = (saved); } while (0)
#define irqs_disabled_flags(saved) ((saved) & 128)
#define isb() ((void)0)
#define READ_ONCE(x) (assert(&(x) == &page->syscall_ns || (irq_flags & 128)), (x))
#define WRITE_ONCE(x, v) do { assert(irq_flags & 128); (x) = (v); } while (0)
static void writeq(unsigned long value, void *address) {
    assert(value == 1 && address == registers + 24);
    assert(irq_flags == 0);
    assert(page->last == page->bound);
    ++writes;
    page->now = page->last;
    page->bound += window;
    if (next_cost) page->syscall_ns = next_cost;
}
