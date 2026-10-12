/* Reproducer: a safepoint poll faults inside a code-cache fragment that
 * DynamoRIO flushed (FRAG_WAS_DELETED) while the thread was still inside it.
 *
 * Shape (mirrors the Kafka/HotSpot crash):
 *   - an RWX "code heap" holding one JIT block on page 0:
 *         ldr x9, [x10]          ; park point (userfaultfd page in uffd mode)
 *         <filler adds>          ; long body in preempt mode
 *         ldr x8, [x28, #1112]   ; HotSpot: JavaThread->polling_page
 *         ldr wzr, [x8]          ; safepoint poll
 *         ret
 *     plus a never-re-executed dummy block on page 1. Executing both once at
 *     warmup makes DR write-protect pages 0-1, so the compiler thread's write
 *     to page 1 faults into DR and flushes the whole code-heap vmarea, like a
 *     C1 compiler writing a new nmethod into the profiled code heap.
 *   - main thread executes the block with x28 = fake JavaThread;
 *   - a "compiler" thread writes to page 1 of the code heap (DR flushes the
 *     whole merged code-heap vmarea) and arms the poll page with PROT_NONE
 *     while main is parked inside the fragment, then releases main;
 *   - main's SIGSEGV handler checks that uc_mcontext.pc is the poll instruction
 *     and that x28 is the fake JavaThread (HotSpot's get_poll_stub(pc) contract).
 *
 * Modes:
 *   call     main parks deterministically inside a clean call made from the
 *            block's entry by the scratch "park" client (/r/librepro_park.so):
 *            the client blocks on a futex while the compiler thread flushes
 *            and arms, mirroring main parked in the progress client's getpid.
 *            With no client loaded there is no park; the compiler times out and
 *            flushes+arms before main runs, so native runs are a passing control.
 *            With the CLOBBER argument the compiler also rewrites the block's
 *            first instruction (plain load -> exclusive load) after the flush:
 *            the flushed page is writable again, so DR sees no fault and the
 *            deleted fragment's app code diverges from its cache copy (like a
 *            ro2sandboxed code-heap page that kept accepting JIT writes).
 *            Re-decoding the block at fault time then mismatches the cache,
 *            which is what fails translation in the real crash.
 *   uffd     main parks on a userfaultfd page inside the block. Documented
 *            variant; DR hangs when the flush happens while main is parked in
 *            the page fault (kept for reference).
 *   before   control: flush + arm happen before main enters the block.
 *   preempt  the process pins itself to CPU 0; main is preempted at an arbitrary
 *            point of a long block while the compiler thread flushes/arms.
 *
 * The compiler thread never returns (pthread_exit under stock DR hangs this
 * static musl binary; see REPORT.md). main finishes with exit_group.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <linux/userfaultfd.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

#ifndef UFFD_USER_MODE_ONLY
#define UFFD_USER_MODE_ONLY 1
#endif

enum { PAGE = 4096, HEAP_PAGES = 256, POLL_OFFSET = 1112 };
enum mode { MODE_UFFD, MODE_BEFORE, MODE_PREEMPT, MODE_CALL };

/* First block instruction: normally a plain load; the compiler rewrites it
 * after the flush, so re-decoding the deleted fragment mismatches its cache
 * copy. CLOBBER=1 uses an exclusive load (multi-instruction mangling: the
 * poll cache pc is silently mis-mapped to the wrong app pc); CLOBBER=2 uses
 * a branch (the re-decoded block ends immediately: translation fails
 * outright and DR delivers pc=0, the real crash's signature). */
#define BLOCK_FIRST_NORMAL 0xf9400149 /* ldr x9, [x10] */
#define BLOCK_FIRST_LDXR 0xc85f7d49   /* ldxr x9, [x10] */
#define BLOCK_FIRST_BSELF 0x14000000  /* b . */

static enum mode mode;
static int clobber;
static uint32_t *heap;          /* RWX code heap: block on page 0, dummy on page 1 */
static uint32_t *poll_insn;     /* address of `ldr wzr, [x8]` */
static uint8_t *poll_page;
static uint8_t *park_page;      /* uffd-registered page (or plain page) */
static uint64_t *fake_thread;   /* fake JavaThread: [1112/8] = poll_page */
static int uffd = -1;
static unsigned filler;

static atomic_uint go, done;
static atomic_int stop;
static atomic_ulong polls_good, polls_bad, polls_total;
static uint64_t first_bad_pc = 1, first_bad_x28;
static sigjmp_buf recover;

/* Park handshake for MODE_CALL, shared with the park client (see park.c).
 * park_req: main sets it to the iteration number before entering the block;
 *           the client's clean call sees it nonzero and parks; the compiler
 *           thread clears it to release main.
 * park_ack: the client sets it to park_req once parked inside the clean call. */
static atomic_uint park_req, park_ack;

static void die(const char *what)
{
    fprintf(stderr, "repro: %s: %s\n", what, strerror(errno));
    exit(2);
}

static void segv(int sig, siginfo_t *info, void *context)
{
    (void)sig;
    ucontext_t *uc = context;
    if ((uint8_t *)info->si_addr != poll_page) {
        static const char msg[] = "repro: unexpected SIGSEGV\n";
        write(2, msg, sizeof(msg) - 1);
        fprintf(stderr, "addr=%p pc=%llx\n", info->si_addr,
                (unsigned long long)uc->uc_mcontext.pc);
        _exit(3);
    }
    atomic_fetch_add(&polls_total, 1);
    mprotect(poll_page, PAGE, PROT_READ);
    uint64_t pc = uc->uc_mcontext.pc, x28 = uc->uc_mcontext.regs[28];
    if (pc == (uint64_t)(uintptr_t)poll_insn && x28 == (uint64_t)(uintptr_t)fake_thread) {
        atomic_fetch_add(&polls_good, 1);
        return; /* resume at the poll, now readable */
    }
    if (atomic_fetch_add(&polls_bad, 1) == 0) {
        first_bad_pc = pc;
        first_bad_x28 = x28;
    }
    siglongjmp(recover, 1);
}

static void emit(void)
{
    uint32_t *p = heap;
    *p++ = BLOCK_FIRST_NORMAL;
    for (unsigned i = 0; i < filler; ++i)
        *p++ = 0x91000400; /* add x0, x0, #1 */
    *p++ = 0xf9422f88; /* ldr x8, [x28, #1112] */
    poll_insn = p;
    *p++ = 0xb940011f; /* ldr wzr, [x8] */
    *p++ = 0xd65f03c0; /* ret */
    /* Dummy block on page 1: executed once so DR translates (and write-
     * protects) page 1, giving the compiler thread's jit_write a protected
     * page to fault on, like the JVM writing a different code-heap page. */
    uint32_t *d = heap + PAGE / 4;
    *d = 0xd65f03c0; /* ret */
    __builtin___clear_cache((char *)heap, (char *)(heap + 2 * (PAGE / 4)));
}

static void run_block(void)
{
    __asm__ volatile("mov x10, %0\n\t"
                     "mov x11, x28\n\t"
                     "mov x28, %1\n\t"
                     "blr %2\n\t"
                     "mov x28, x11\n\t"
                     :
                     : "r"(park_page), "r"(fake_thread), "r"(heap)
                     : "x0", "x8", "x9", "x10", "x11", "x30", "memory", "cc");
}

static void run_dummy(void)
{
    __asm__ volatile("blr %0\n\t" ::"r"(heap + PAGE / 4) : "x30", "memory", "cc");
}

/* A store to the translated dummy page of the code heap, as a JIT installing
 * a new method on a different page of the same code-heap region. */
static void jit_write(unsigned iteration)
{
    volatile uint32_t *target = heap + PAGE / 4 + 64 + (iteration % 64);
    *target = 0xd503201f; /* nop */
}

static void arm(void) { if (mprotect(poll_page, PAGE, PROT_NONE)) die("arm"); }

static int futex_wait(atomic_uint *addr, unsigned val, const struct timespec *ts)
{
    return syscall(SYS_futex, (unsigned *)addr, FUTEX_WAIT_PRIVATE, val, ts, NULL, 0);
}

static void futex_wake(atomic_uint *addr)
{
    syscall(SYS_futex, (unsigned *)addr, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
}

static void *compiler(void *arg)
{
    unsigned iterations = (unsigned)(uintptr_t)arg;
    if (mode == MODE_PREEMPT) {
        struct timespec nap = {0, 300 * 1000};
        for (unsigned i = 0; i < iterations; ++i) {
            nanosleep(&nap, NULL);
            jit_write(i);
            arm();
            nanosleep(&nap, NULL);
            /* Disarm if main has not polled yet so each arm yields <= 1 fault. */
            mprotect(poll_page, PAGE, PROT_READ);
        }
    } else if (mode == MODE_CALL) {
        for (unsigned i = 1; i <= iterations; ++i) {
            /* Wait until the client's clean call reports main parked inside
             * the fragment. Without a client (native control) there is no
             * park: time out and flush+arm before main runs instead. */
            unsigned a;
            while ((a = atomic_load(&park_ack)) != i) {
                struct timespec ts = {0, 20 * 1000 * 1000};
                if (futex_wait(&park_ack, a, &ts) < 0 && errno == ETIMEDOUT)
                    break;
            }
            jit_write(i);
            arm();
            if (clobber) {
                /* The flush made the heap writable again: rewrite the block's
                 * first instruction without DR noticing, so its deleted
                 * fragment no longer matches a re-decode of the app page. */
                heap[0] = clobber == 2 ? BLOCK_FIRST_BSELF : BLOCK_FIRST_LDXR;
                __builtin___clear_cache((char *)heap, (char *)(heap + 1));
            }
            atomic_store(&park_req, 0);
            futex_wake(&park_req);
        }
    } else {
        for (unsigned i = 1; i <= iterations; ++i) {
            if (mode == MODE_UFFD) {
                if (madvise(park_page, PAGE, MADV_DONTNEED)) die("madvise");
            } else {
                jit_write(i);
                arm();
            }
            atomic_store(&go, i);
            if (mode == MODE_UFFD) {
                struct uffd_msg msg;
                for (;;) {
                    ssize_t n = read(uffd, &msg, sizeof(msg));
                    if (n == sizeof(msg)) break;
                    if (n < 0 && errno == EINTR) continue;
                    die("uffd read");
                }
                if (msg.event != UFFD_EVENT_PAGEFAULT) { fprintf(stderr, "uffd event %d\n", msg.event); exit(2); }
                /* main is now parked inside the fragment */
                jit_write(i);
                arm();
                struct uffdio_zeropage zero = {.range = {(uintptr_t)park_page, PAGE}};
                if (ioctl(uffd, UFFDIO_ZEROPAGE, &zero)) die("UFFDIO_ZEROPAGE");
            }
            while (atomic_load(&done) != i) sched_yield();
        }
    }
    atomic_store(&stop, 1);
    /* Never return: pthread_exit of a thread hangs this static musl binary
     * under stock DR. main ends the process with exit_group. */
    for (;;)
        pause();
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: repro call|uffd|before|preempt ITERATIONS [FILLER] [CLOBBER]\n");
        return 2;
    }
    if (!strcmp(argv[1], "uffd")) mode = MODE_UFFD;
    else if (!strcmp(argv[1], "before")) mode = MODE_BEFORE;
    else if (!strcmp(argv[1], "preempt")) mode = MODE_PREEMPT;
    else if (!strcmp(argv[1], "call")) mode = MODE_CALL;
    else { fprintf(stderr, "bad mode\n"); return 2; }
    unsigned iterations = (unsigned)strtoul(argv[2], NULL, 0);
    filler = argc > 3 ? (unsigned)strtoul(argv[3], NULL, 0) : (mode == MODE_PREEMPT ? 900 : 0);
    clobber = argc > 4 ? (int)strtol(argv[4], NULL, 0) : 0;

    heap = mmap(NULL, HEAP_PAGES * PAGE, PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    poll_page = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    park_page = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    fake_thread = calloc(1, 4096);
    if (heap == MAP_FAILED || poll_page == MAP_FAILED || park_page == MAP_FAILED || !fake_thread)
        die("allocate");
    fake_thread[POLL_OFFSET / 8] = (uint64_t)(uintptr_t)poll_page;
    emit();

    if (mode == MODE_CALL) {
        /* Publish the block and handshake addresses for the park client,
         * which reads them when the block is first translated. */
        FILE *f = fopen("/tmp/repro-park-target", "w");
        if (!f) die("park target");
        fprintf(f, "%lx %lx %lx\n", (unsigned long)(uintptr_t)heap,
                (unsigned long)(uintptr_t)&park_req, (unsigned long)(uintptr_t)&park_ack);
        fclose(f);
    }

    if (mode == MODE_UFFD) {
        uffd = (int)syscall(SYS_userfaultfd, O_CLOEXEC | UFFD_USER_MODE_ONLY);
        if (uffd < 0) die("userfaultfd");
        struct uffdio_api api = {.api = UFFD_API};
        if (ioctl(uffd, UFFDIO_API, &api)) die("UFFDIO_API");
        struct uffdio_register reg = {.range = {(uintptr_t)park_page, PAGE},
                                      .mode = UFFDIO_REGISTER_MODE_MISSING};
        if (ioctl(uffd, UFFDIO_REGISTER, &reg)) die("UFFDIO_REGISTER");
        if (madvise(park_page, PAGE, MADV_DONTNEED)) die("madvise");
    }

    struct sigaction sa = {0};
    sa.sa_sigaction = segv;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, NULL)) die("sigaction");

    /* Warm up: execute both blocks once so DR builds them and write-protects
     * the heap pages. */
    if (mode != MODE_UFFD) { run_block(); run_dummy(); }

    if (mode == MODE_PREEMPT) {
        /* One CPU, like Seedbox's single guest vCPU: main can be descheduled
         * mid-block while the compiler thread runs. */
        cpu_set_t one;
        CPU_ZERO(&one);
        CPU_SET(0, &one);
        if (sched_setaffinity(0, sizeof(one), &one)) die("sched_setaffinity");
    }

    pthread_t thread;
    if (pthread_create(&thread, NULL, compiler, (void *)(uintptr_t)iterations)) die("pthread_create");

    unsigned long runs = 0;
    if (mode == MODE_PREEMPT) {
        sigsetjmp(recover, 1);
        while (!atomic_load(&stop)) { run_block(); runs++; }
    } else if (mode == MODE_CALL) {
        for (unsigned i = 1; i <= iterations; ++i) {
            if (clobber) {
                /* Restore the block's first instruction before DR rebuilds
                 * it. The heap is still writable from the last flush, so this
                 * write does not fault into DR (the first iteration's write
                 * faults once, flushing the warmup fragments: harmless). */
                heap[0] = BLOCK_FIRST_NORMAL;
                __builtin___clear_cache((char *)heap, (char *)(heap + 1));
            }
            atomic_store(&park_req, i);
            if (!sigsetjmp(recover, 1)) run_block();
            runs++;
            while (atomic_load(&park_req) != 0) sched_yield();
        }
    } else {
        for (unsigned i = 1; i <= iterations; ++i) {
            while (atomic_load(&go) != i) sched_yield();
            if (!sigsetjmp(recover, 1)) run_block();
            runs++;
            atomic_store(&done, i);
        }
    }
    unsigned long total = atomic_load(&polls_total), good = atomic_load(&polls_good),
                  bad = atomic_load(&polls_bad);
    printf("mode=%s iterations=%u filler=%u runs=%lu poll_faults=%lu good=%lu bad=%lu",
           argv[1], iterations, filler, runs, total, good, bad);
    if (bad)
        printf(" first_bad_pc=%#llx first_bad_x28=%#llx expected_pc=%p expected_x28=%p",
               (unsigned long long)first_bad_pc, (unsigned long long)first_bad_x28,
               (void *)poll_insn, (void *)fake_thread);
    printf("\n");
    fflush(stdout);
    /* exit_group: the compiler thread is parked in pause() and must not go
     * through pthread_exit (hangs under stock DR). */
    syscall(SYS_exit_group, bad ? 1 : 0);
    return 1;
}
