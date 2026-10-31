/* Host model for the ARM guest kernel's syscall charging (guest/kernel/arm64/seedbox.c).
 * Each rendezvous publishes the guest's clock and opens a further window. */
#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

typedef uint64_t u64;
#define notrace
#define __iomem
#define unlikely(x) (x)
#define panic(...) abort()
#define max(a, b) ((a) > (b) ? (a) : (b))
struct pt_regs { unsigned long regs[31]; };
struct clock_event_device { int unused; };
struct task_struct { int pid; };
static struct task_struct tasks[2], *current = &tasks[0];
#define HARDIRQ_OFFSET (1UL << 16)
#define SOFTIRQ_OFFSET (1UL << 8)
static unsigned long preempt, nesting;
#define irq_count() preempt
static unsigned int user_ticks, system_ticks;

static unsigned long irq_flags;
static unsigned int writes;
static u64 window;
static char registers[64];
#define local_irq_save(saved) do { (saved) = irq_flags; irq_flags |= 128; } while (0)
#define local_irq_enable() do { irq_flags &= ~128UL; } while (0)
#define local_irq_disable() do { irq_flags |= 128; } while (0)
#define local_irq_restore(saved) do { irq_flags = (saved); } while (0)
#define irqs_disabled_flags(saved) ((saved) & 128)
#define isb() ((void)0)
#define READ_ONCE(x) (x)
#define WRITE_ONCE(x, v) do { (x) = (v); } while (0)
static void writeq(unsigned long value, volatile void *address);

#include "charge.c"

static struct seedbox_page storage;

static void writeq(unsigned long value, volatile void *address)
{
	/* Only the unmasked yield rendezvous is reachable from a charge. */
	assert(value == 1 && address == (void *)(registers + 24));
	assert(irq_flags == 0);
	assert(storage.last == storage.bound);
	++writes;
	storage.now = storage.last;
	storage.bound += window;
	/* Each rendezvous takes a tick in hard IRQ context on top of `nesting`. */
	preempt = nesting + HARDIRQ_OFFSET;
	if (seedbox_user_tick())
		++user_ticks;
	else
		++system_ticks;
	preempt = 0;
}

static struct pt_regs regs;

static void reset(u64 cost, u64 bound, u64 next_window)
{
	storage = (struct seedbox_page){ .bound = bound, .syscall_ns = cost, .abi = 3 };
	page = &storage;
	event = registers;
	window = next_window;
	writes = user_ticks = system_ticks = 0;
	nesting = 0;
	current = &tasks[0];
	regs = (struct pt_regs){ { 0 } };
}

int main(void)
{
	(void)&ce; /* registration state outside the charge model */
	/* An application syscall charges the published price. */
	reset(1000, UINT64_MAX, 0);
	seedbox_syscall_progress(&regs, 63);
	assert(storage.last == 1000 && writes == 0);

	/* Time syscalls stand in for vDSO reads: only their own clock read costs. */
	reset(1000, UINT64_MAX, 0);
	seedbox_syscall_progress(&regs, SEEDBOX_CLOCK_GETTIME_NR);
	seedbox_syscall_progress(&regs, SEEDBOX_CLOCK_GETRES_NR);
	seedbox_syscall_progress(&regs, SEEDBOX_GETTIMEOFDAY_NR);
	assert(storage.last == 0 && writes == 0);
	(void)seedbox_read();
	assert(storage.last == 1);

	/* The translator's own syscalls cost a tenth, scaled by throttle. */
	reset(1000, UINT64_MAX, 0);
	regs.regs[7] = SEEDBOX_TRANSLATOR_MARK;
	seedbox_syscall_progress(&regs, 98);
	assert(storage.last == 100);
	storage.syscall_ns = 4000;
	seedbox_syscall_progress(&regs, 98);
	assert(storage.last == 500);
	/* Still at least one bounded read, so pending interrupts are delivered. */
	storage.syscall_ns = 5;
	seedbox_syscall_progress(&regs, 98);
	assert(storage.last == 501);
	/* Another x7 value is an application syscall at full price. */
	regs.regs[7] = SEEDBOX_TRANSLATOR_MARK + 1;
	seedbox_syscall_progress(&regs, 98);
	assert(storage.last == 506);

	/* A charge crossing the bound yields there and continues in later windows. */
	reset(1000, 300, 300);
	seedbox_syscall_progress(&regs, 63);
	assert(storage.last == 1000 && writes == 3);
	/* Ticks during an ordinary syscall's charge are system time. */
	assert(user_ticks == 0 && system_ticks == 3);

	/* A progress call charges the same, but its ticks are user time. */
	reset(1000, 300, 300);
	regs.regs[0] = SEEDBOX_PROGRESS_MARK;
	seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
	assert(storage.last == 1000 && user_ticks == 3 && system_ticks == 0);
	/* Only while the charge runs: a later read's tick is system time. */
	storage.last = storage.bound;
	(void)seedbox_read();
	assert(user_ticks == 3 && system_ticks == 1);
	/* getpid without the mark, and the mark on another syscall, are ordinary. */
	seedbox_syscall_progress(&(struct pt_regs){ { 0 } }, SEEDBOX_PROGRESS_NR);
	seedbox_syscall_progress(&regs, 63);
	assert(user_ticks == 3);
	/* A tick nested in a softirq is not user time. */
	reset(1000, 300, 300);
	regs.regs[0] = SEEDBOX_PROGRESS_MARK;
	nesting = SOFTIRQ_OFFSET;
	seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
	assert(user_ticks == 0 && system_ticks == 3);
	return 0;
}
