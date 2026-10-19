/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/sigregs.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/*
 * Register integrity under thread-directed signals (AArch64). Worker threads load a known
 * pattern into x0-x17, x19-x26 and q0-q31, spin, then store and compare everything, while
 * the main thread sends them SIGHUP with tgkill and waits for each handler, which posts a
 * semaphore the way HotSpot's user-signal handler does. Natively it prints "OK".
 *
 * Usage: sigregs WORKERS SIGNALS [SPINS [x28|ibl]]
 */
#define _GNU_SOURCE
#include "support.h"
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define NX 26
#define NQ 32

struct block {
    uint64_t *ex, *sx, *eq, *sq;
    uint64_t spins;
    uint64_t *area;
    uint64_t x28_mode;
};

struct worker {
    uint64_t expect_x[NX], seen_x[NX], expect_q[NQ * 2], seen_q[NQ * 2], area[2];
    struct block b;
    pid_t tid;
    long bad, rounds;
    pthread_t thread;
};

static sem_t handled;
static volatile int done;
static volatile long handler_calls;

static void handler(int sig, siginfo_t *info, void *uc) {
    (void)sig; (void)info; (void)uc;
    handler_calls++;
    sem_post(&handled);
}

static void run(struct block *b) {
    register void *block __asm__("x27") = b;
    __asm__ volatile(
        "stp x29, x30, [sp, #-16]!\n"
        /* q0-q31 */
        "ldr x29, [x27, #16]\n"
        "ldp q0, q1, [x29, #0]\n" "ldp q2, q3, [x29, #32]\n" "ldp q4, q5, [x29, #64]\n"
        "ldp q6, q7, [x29, #96]\n" "ldp q8, q9, [x29, #128]\n" "ldp q10, q11, [x29, #160]\n"
        "ldp q12, q13, [x29, #192]\n" "ldp q14, q15, [x29, #224]\n" "ldp q16, q17, [x29, #256]\n"
        "ldp q18, q19, [x29, #288]\n" "ldp q20, q21, [x29, #320]\n" "ldp q22, q23, [x29, #352]\n"
        "ldp q24, q25, [x29, #384]\n" "ldp q26, q27, [x29, #416]\n" "ldp q28, q29, [x29, #448]\n"
        "ldp q30, q31, [x29, #480]\n"
        "ldr x28, [x27, #32]\n"
        "ldr x29, [x27, #48]\n"
        "cmp x29, #1\n"
        "b.ne 2f\n"
        "ldr x29, [x27, #40]\n"
        "str x28, [x29]\n"
        "mov x28, x29\n"
        "2:\n"
        "ldr x29, [x27, #0]\n"
        "ldp x0, x1, [x29, #0]\n" "ldp x2, x3, [x29, #16]\n" "ldp x4, x5, [x29, #32]\n"
        "ldp x6, x7, [x29, #48]\n" "ldp x8, x9, [x29, #64]\n" "ldp x10, x11, [x29, #80]\n"
        "ldp x12, x13, [x29, #96]\n" "ldp x14, x15, [x29, #112]\n" "ldp x16, x17, [x29, #128]\n"
        "ldp x19, x20, [x29, #144]\n" "ldp x21, x22, [x29, #160]\n" "ldp x23, x24, [x29, #176]\n"
        "ldp x25, x26, [x29, #192]\n"
        "ldr x29, [x27, #48]\n"
        "cmp x29, #2\n"
        "b.eq 5f\n"
        "cbnz x29, 3f\n"
        "1:\n"
        "subs x28, x28, #1\n"
        "b.ne 1b\n"
        "b 4f\n"
        "3:\n"
        "ldr x29, [x28, #0]\n"
        "subs x29, x29, #1\n"
        "str x29, [x28, #0]\n"
        "ldr x29, [x28, #8]\n"
        "add x29, x29, #3\n"
        "str x29, [x28, #8]\n"
        "ldr x29, [x28, #0]\n"
        "cbnz x29, 3b\n"
        /* Mode 2: every iteration calls and returns through registers, so signals
         * also land in indirect-branch lookup and target prologues. */
        "5:\n"
        "adr x29, 6f\n"
        "blr x29\n"
        "subs x28, x28, #1\n"
        "b.ne 5b\n"
        "b 4f\n"
        "6:\n"
        "ret\n"
        "4:\n"
        "ldr x29, [x27, #8]\n"
        "stp x0, x1, [x29, #0]\n" "stp x2, x3, [x29, #16]\n" "stp x4, x5, [x29, #32]\n"
        "stp x6, x7, [x29, #48]\n" "stp x8, x9, [x29, #64]\n" "stp x10, x11, [x29, #80]\n"
        "stp x12, x13, [x29, #96]\n" "stp x14, x15, [x29, #112]\n" "stp x16, x17, [x29, #128]\n"
        "stp x19, x20, [x29, #144]\n" "stp x21, x22, [x29, #160]\n" "stp x23, x24, [x29, #176]\n"
        "stp x25, x26, [x29, #192]\n"
        "ldr x29, [x27, #24]\n"
        "stp q0, q1, [x29, #0]\n" "stp q2, q3, [x29, #32]\n" "stp q4, q5, [x29, #64]\n"
        "stp q6, q7, [x29, #96]\n" "stp q8, q9, [x29, #128]\n" "stp q10, q11, [x29, #160]\n"
        "stp q12, q13, [x29, #192]\n" "stp q14, q15, [x29, #224]\n" "stp q16, q17, [x29, #256]\n"
        "stp q18, q19, [x29, #288]\n" "stp q20, q21, [x29, #320]\n" "stp q22, q23, [x29, #352]\n"
        "stp q24, q25, [x29, #384]\n" "stp q26, q27, [x29, #416]\n" "stp q28, q29, [x29, #448]\n"
        "stp q30, q31, [x29, #480]\n"
        "ldp x29, x30, [sp], #16\n"
        :
        : "r"(block)
        : "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13",
          "x14", "x15", "x16", "x17", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26",
          "x28", "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10", "v11",
          "v12", "v13", "v14", "v15", "v16", "v17", "v18", "v19", "v20", "v21", "v22", "v23",
          "v24", "v25", "v26", "v27", "v28", "v29", "v30", "v31", "memory", "cc");
}


static void *work(void *p) {
    struct worker *w = p;
    w->tid = (pid_t)syscall(SYS_gettid);
    for (long r = 0; !done; r++) {
        for (int i = 0; i < NX; i++) w->expect_x[i] = 0x1000000000000000ULL * (i % 15 + 1) + r * 0x10001ULL + i + ((uint64_t)w->tid << 20);
        for (int i = 0; i < NQ * 2; i++) w->expect_q[i] = 0xA5A5000000000000ULL ^ ((uint64_t)i << 32) ^ (r * 0x9E3779B9ULL);
        memset(w->seen_x, 0, sizeof(w->seen_x));
        memset(w->seen_q, 0, sizeof(w->seen_q));
        run(&w->b);
        for (int i = 0; i < NX; i++)
            if (w->seen_x[i] != w->expect_x[i] && w->bad++ < 10)
                fprintf(stderr, "tid %d round %ld: x%d = %#llx, expected %#llx\n", w->tid, r, i < 18 ? i : i + 1,
                        (unsigned long long)w->seen_x[i], (unsigned long long)w->expect_x[i]);
        for (int i = 0; i < NQ * 2; i++)
            if (w->seen_q[i] != w->expect_q[i] && w->bad++ < 10)
                fprintf(stderr, "tid %d round %ld: q%d.%s = %#llx, expected %#llx\n", w->tid, r, i / 2, i % 2 ? "hi" : "lo",
                        (unsigned long long)w->seen_q[i], (unsigned long long)w->expect_q[i]);
        w->rounds++;
    }
    return NULL;
}

int main(int argc, char **argv) {
    int workers = argc > 1 ? atoi(argv[1]) : 4;
    long signals = argc > 2 ? atol(argv[2]) : 2000;
    uint64_t spins = argc > 3 ? strtoull(argv[3], NULL, 0) : 20000;
    int x28 = argc > 4 ? (!strcmp(argv[4], "x28") ? 1 : !strcmp(argv[4], "ibl") ? 2 : 0) : 0;
    sem_init(&handled, 0, 0);
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGHUP, &sa, NULL);
    struct worker *ws = calloc(workers, sizeof(*ws));
    for (int i = 0; i < workers; i++) {
        struct worker *w = &ws[i];
        w->b = (struct block){w->expect_x, w->seen_x, w->expect_q, w->seen_q, spins, w->area, x28};
        pthread_create(&w->thread, NULL, work, w);
    }
    for (int i = 0; i < workers; i++) while (__atomic_load_n(&ws[i].tid, __ATOMIC_ACQUIRE) == 0) sched_yield();
    pid_t pid = getpid();
    for (long s = 0; s < signals; s++) {
        syscall(SYS_tgkill, pid, ws[s % workers].tid, SIGHUP);
        sem_wait(&handled);
        if (getenv("SIGREGS_TRACE")) fprintf(stderr, "handled %ld\n", s);
    }
    if (getenv("SIGREGS_TRACE")) fprintf(stderr, "joining\n");
    done = 1;
    long bad = 0, rounds = 0;
    for (int i = 0; i < workers; i++) { pthread_join(ws[i].thread, NULL); bad += ws[i].bad; rounds += ws[i].rounds; }
    if (handler_calls != signals) { printf("BAD handled %ld of %ld\n", handler_calls, signals); return 1; }
    printf(bad ? "BAD %ld\n" : "OK\n", bad);
    fprintf(stderr, "rounds %ld\n", rounds);
    return bad != 0 || rounds == 0;
}
