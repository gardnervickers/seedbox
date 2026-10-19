/* External public-API port of the transitions/ordered-pipe/signal/checksum
 * invariants in DR 356c1821045a4b5678d0813e132074324623df6e
 * suite/tests/api/a64_prop_lifecycle.c and detach_state.c (emit flavors).
 * The generated ISA block oracle is accounted separately in inventory.tsv. */
#define _GNU_SOURCE
#include "dr_api.h"
#include "dr_app.h"
#include "support.h"
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/wait.h>

static _Atomic int stop, started, in_handler, leave_handler;
static _Atomic long runs, signal_runs;
static _Atomic int errors;
static int data_pipe[2], receipt[2];
static volatile pid_t worker_tid;
static _Atomic int emit_default, emit_stored, instrumented;
static void counted(void) { dr_atomic_add32_return_sum((volatile int *)&instrumented, 1); }
static dr_emit_flags_t block(void *dc, void *tag, instrlist_t *bb, bool trace, bool translating) {
    (void)trace; (void)translating;
    instr_t *first = instrlist_first_app(bb);
    if (first) dr_insert_clean_call(dc, bb, first, (void *)counted, false, 0);
    /* Source fix: instruction-aligned tags must select BOTH emit flavors on A64. */
    ptr_uint_t bits = (ptr_uint_t)tag;
#ifdef AARCH64
    bits >>= 2;
#endif
    if (bits & 1) {
        dr_atomic_add32_return_sum((volatile int *)&emit_stored, 1);
        return DR_EMIT_STORE_TRANSLATIONS;
    }
    dr_atomic_add32_return_sum((volatile int *)&emit_default, 1);
    return DR_EMIT_DEFAULT;
}
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[]) {
    (void)id; (void)argc; (void)argv;
    dr_register_bb_event(block);
}
static void handler(int sig, siginfo_t *si, void *uv) {
    (void)sig; (void)si; (void)uv;
    volatile uint64_t sum = 0;
    atomic_store(&in_handler, 1);
    while (!atomic_load(&leave_handler)) {
        sum = 0;
        for (unsigned i = 1; i <= 20000; i++) sum += (uint64_t)i * i;
        if (sum != UINT64_C(2666866670000)) atomic_fetch_add(&errors, 1);
    }
    atomic_fetch_add(&signal_runs, 1);
}
static void *compute(void *arg) {
    (void)arg;
    worker_tid = gettid();
    atomic_store(&started, 1);
    while (!atomic_load(&stop)) {
        volatile uint64_t sum = 0;
        for (unsigned i = 1; i <= 20000; i++) sum += (uint64_t)i * i;
        if (sum != UINT64_C(2666866670000)) atomic_fetch_add(&errors, 1);
        atomic_fetch_add(&runs, 1);
    }
    return NULL;
}
static void *reader(void *arg) {
    (void)arg;
    for (unsigned want = 1; want <= 24; want++) {
        unsigned got = 0;
        if (read(data_pipe[0], &got, sizeof got) != sizeof got || got != want)
            atomic_fetch_add(&errors, 1);
        if (write(receipt[1], &got, sizeof got) != sizeof got) abort();
    }
    return NULL;
}
static void operation(unsigned v) {
    unsigned got = 0;
    if (write(data_pipe[1], &v, sizeof v) != sizeof v ||
        read(receipt[0], &got, sizeof got) != sizeof got || got != v) abort();
}
int main(int argc, char **argv) {
    int native = argc == 2 && !strcmp(argv[1], "native");
    if (argc != 2 || (!native && strcmp(argv[1], "attach"))) return 2;
    struct sigaction a = {0}; a.sa_sigaction = handler; a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&a.sa_mask); if (sigaction(SIGUSR2, &a, NULL)) return 2;
    if (pipe(data_pipe) || pipe(receipt)) return 2;
    pthread_t comp, rd;
    if (pthread_create(&comp, NULL, compute, NULL) || pthread_create(&rd, NULL, reader, NULL)) return 2;
    while (!atomic_load(&started)) sched_yield();
    unsigned seq = 0;
    for (int round = 0; round < 6; round++) {
        /* A handler is positively live at each boundary. No signal is sent in
         * the setup window: an attach-time dropped signal is never excused. */
        atomic_store(&in_handler, 0); atomic_store(&leave_handler, 0);
        if (syscall(SYS_tgkill, getpid(), worker_tid, SIGUSR2)) return 2;
        while (!atomic_load(&in_handler)) sched_yield();
        operation(++seq);
        if (!native) {
            if (dr_app_setup_and_start() || !dr_app_running_under_dynamorio()) abort();
        }
        operation(++seq);
        atomic_store(&leave_handler, 1);
        long want = round * 2 + 1;
        while (atomic_load(&signal_runs) < want) sched_yield();
        long before = atomic_load(&runs);
        while (atomic_load(&runs) <= before) sched_yield();
        atomic_store(&in_handler, 0); atomic_store(&leave_handler, 0);
        if (syscall(SYS_tgkill, getpid(), worker_tid, SIGUSR2)) return 2;
        while (!atomic_load(&in_handler)) sched_yield();
        operation(++seq);
        if (!native) {
            dr_app_stop_and_cleanup();
            if (dr_app_running_under_dynamorio()) abort();
        }
        operation(++seq);
        atomic_store(&leave_handler, 1);
        while (atomic_load(&signal_runs) < want + 1) sched_yield();
        before = atomic_load(&runs);
        while (atomic_load(&runs) <= before) sched_yield();
    }
    atomic_store(&stop, 1);
    pthread_join(comp, NULL); pthread_join(rd, NULL);
    if (!native && (!atomic_load(&emit_default) || !atomic_load(&emit_stored) || !atomic_load(&instrumented))) abort();
    if (atomic_load(&errors) || atomic_load(&signal_runs) != 12) abort();
    puts("OK lifecycle: 12 transitions, 24 ordered operations, 12 live handlers");
    return 0;
}
