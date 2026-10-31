/* Host model for the x86 guest kernel's syscall charging (guest/kernel/x86_64/seedbox.c).
 * Each yield publishes the guest's clock, opens a further window and takes a tick. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

typedef uint64_t u64;
typedef uint16_t u16;
#define noinstr
#define unlikely(x) (x)
#define SEEDBOX_YIELD_PORT 0x501
struct pt_regs { unsigned long di; };
struct clock_event_device { int unused; };
struct task_struct { int pid; };
static struct task_struct tasks[2], *current = &tasks[0];
#define HARDIRQ_OFFSET (1UL << 16)
#define SOFTIRQ_OFFSET (1UL << 8)
static unsigned long preempt, nesting;
#define irq_count() preempt
static unsigned int user_ticks, system_ticks, yields;
static unsigned long irq_flags = 0x200;
#define arch_local_irq_save() ({ unsigned long saved = irq_flags; irq_flags &= ~0x200UL; saved; })
#define arch_local_irq_restore(saved) do { irq_flags = (saved); } while (0)
#define arch_local_irq_disable() do { irq_flags &= ~0x200UL; } while (0)
#define arch_irqs_disabled_flags(saved) (!((saved) & 0x200))
#define READ_ONCE(x) (x)
#define WRITE_ONCE(x, v) do { (x) = (v); } while (0)
static u64 window;
static void event_signal(u16 port);

#include "charge.c"

static struct seedbox_page storage;

static void event_signal(u16 port)
{
	assert(port == SEEDBOX_YIELD_PORT);
	/* Only the unmasked yield is reachable from a charge. */
	assert(irq_flags & 0x200);
	assert(storage.last == storage.bound);
	++yields;
	storage.now = storage.last;
	storage.bound += window;
	/* The due timer interrupt arrives right at the yield. */
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
	window = next_window;
	yields = user_ticks = system_ticks = 0;
	nesting = 0;
	regs = (struct pt_regs){ 0 };
}

int main(void)
{
	(void)&ce;
	(void)&event;
	/* An ordinary syscall charges its price; its ticks are system time. */
	reset(1000, 300, 300);
	seedbox_syscall_progress(&regs, 0);
	assert(storage.last == 1000 && yields == 3);
	assert(user_ticks == 0 && system_ticks == 3);

	/* A marked progress getpid charges the same; its ticks are user time. */
	reset(1000, 300, 300);
	regs.di = SEEDBOX_PROGRESS_MARK;
	seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
	assert(storage.last == 1000 && user_ticks == 3 && system_ticks == 0);
	/* Only during the charge: a later clock read's tick is system time. */
	storage.last = storage.bound;
	(void)seedbox_read();
	assert(user_ticks == 3 && system_ticks == 1);
	/* getpid without the mark, and the mark on another syscall, are ordinary. */
	seedbox_syscall_progress(&(struct pt_regs){ 0 }, SEEDBOX_PROGRESS_NR);
	seedbox_syscall_progress(&regs, 0);
	assert(user_ticks == 3);
	/* Time syscalls charge nothing: only their own clock read costs. */
	reset(1000, 300, 300);
	seedbox_syscall_progress(&regs, SEEDBOX_GETTIMEOFDAY_NR);
	seedbox_syscall_progress(&regs, SEEDBOX_TIME_NR);
	seedbox_syscall_progress(&regs, SEEDBOX_CLOCK_GETTIME_NR);
	seedbox_syscall_progress(&regs, SEEDBOX_CLOCK_GETRES_NR);
	assert(storage.last == 0 && yields == 0);
	/* A tick nested in a softirq is not user time. */
	reset(1000, 300, 300);
	regs.di = SEEDBOX_PROGRESS_MARK;
	nesting = SOFTIRQ_OFFSET;
	seedbox_syscall_progress(&regs, SEEDBOX_PROGRESS_NR);
	assert(user_ticks == 0 && system_ticks == 3);
	return 0;
}
