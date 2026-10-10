/* Process churn and spin-waits at the translator boundary.
 * Every phase prints `key: value` lines including its CLOCK_MONOTONIC (virtual)
 * duration; the final digest is busybox sha256sum over all printed lines. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

/* ---- output ---- */
static char trace[8192];
static void emit(const char *fmt, ...) {
    char line[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof line - 1, fmt, ap);
    va_end(ap);
    strcat(line, "\n");
    strncat(trace, line, sizeof trace - strlen(trace) - 1);
    fputs(line, stdout);
    fflush(stdout);
}
static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}
static uint64_t spin(uint64_t n) { /* compute loop: one translated block per iteration */
    volatile uint64_t x = 0;
    for (uint64_t i = 0; i < n; i++) x = x * 6364136223846793005u + i;
    return x;
}

/* a. fork + execve /bin/sh pipelines (DynamoRIO follow-children re-injection) */
static void phase_pipelines(void) {
    char *argv[] = {"sh", "-c", "seq 1 2000 | sort -r | uniq -c | wc -l", NULL};
    int ok = 0;
    uint64_t t0 = now_ns();
    for (int i = 0; i < 30; i++) {
        int fd[2], status;
        char out[64] = {0};
        if (pipe(fd)) exit(2);
        pid_t pid = fork();
        if (pid == 0) {
            dup2(fd[1], 1);
            close(fd[0]);
            close(fd[1]);
            execve("/bin/sh", argv, environ);
            _exit(127);
        }
        close(fd[1]);
        ssize_t n, got = 0;
        while ((n = read(fd[0], out + got, sizeof out - 1 - got)) > 0) got += n;
        close(fd[0]);
        waitpid(pid, &status, 0);
        ok += WIFEXITED(status) && WEXITSTATUS(status) == 0 && atoi(out) == 2000;
    }
    emit("pipeline_ok: %d/30", ok);
    emit("pipeline_ns: %llu", (unsigned long long)(now_ns() - t0));
}

/* b. posix_spawn (clone CLONE_VM|CLONE_VFORK in musl) of /bin/true */
static void phase_spawn(void) {
    char *argv[] = {"true", NULL};
    int ok = 0;
    uint64_t t0 = now_ns();
    for (int i = 0; i < 30; i++) {
        pid_t pid;
        int status;
        if (posix_spawn(&pid, "/bin/true", NULL, NULL, argv, environ) == 0 &&
            waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0)
            ok++;
    }
    emit("spawn_ok: %d/30", ok);
    emit("spawn_ns: %llu", (unsigned long long)(now_ns() - t0));
}

/* c. fork children that keep computing on the inherited block budget */
static void phase_fork_compute(void) {
    char list[400] = "";
    uint64_t t0 = now_ns(), sum = 0;
    for (int i = 0; i < 20; i++) {
        int fd[2], status;
        uint64_t child_ns = 0;
        if (pipe(fd)) exit(2);
        spin(1000u * (uint64_t)i); /* vary the parent's budget position at fork */
        pid_t pid = fork();
        if (pid == 0) {
            uint64_t s = now_ns();
            spin(50000);
            uint64_t d = now_ns() - s;
            if (write(fd[1], &d, sizeof d) != sizeof d) _exit(3);
            _exit(0);
        }
        close(fd[1]);
        if (read(fd[0], &child_ns, sizeof child_ns) != sizeof child_ns) child_ns = 0;
        close(fd[0]);
        waitpid(pid, &status, 0);
        sum += child_ns;
        snprintf(list + strlen(list), sizeof list - strlen(list), "%s%llu",
                 i ? "," : "", (unsigned long long)child_ns);
    }
    emit("fork_child_loop_ns: %s", list);
    emit("fork_child_loop_ns_sum: %llu", (unsigned long long)sum);
    emit("fork_ns: %llu", (unsigned long long)(now_ns() - t0));
}

/* d. 200 short-lived compute threads (fresh per-thread budgets) */
static void *compute_thread(void *arg) {
    return (void *)(uintptr_t)(spin(2000) ^ (uintptr_t)arg);
}
static void phase_threads(void) {
    pthread_t t[200];
    uint64_t t0 = now_ns(), sum = 0;
    int ok = 0;
    for (uintptr_t i = 0; i < 200; i++) ok += pthread_create(&t[i], NULL, compute_thread, (void *)i) == 0;
    for (int i = 0; i < ok; i++) {
        void *r;
        pthread_join(t[i], &r);
        sum += (uintptr_t)r;
    }
    emit("threads_ok: %d/200", ok);
    emit("threads_sum: %016llx", (unsigned long long)sum);
    emit("threads_ns: %llu", (unsigned long long)(now_ns() - t0));
}

/* e. lock-free single-slot mailbox: weak CAS (LL/SC on ARM) + sched_yield */
#define ITEMS 20000
static uint64_t slot, cas_fail[2], cas_spurious[2], yields[2];
static void *producer(void *arg) {
    (void)arg;
    for (uint64_t item = 1; item <= ITEMS; item++) {
        for (;;) {
            uint64_t expect = 0;
            if (__atomic_compare_exchange_n(&slot, &expect, item, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE))
                break;
            cas_fail[0]++;
            cas_spurious[0] += expect == 0;
            yields[0]++;
            sched_yield();
        }
    }
    return NULL;
}
static void phase_mailbox(void) {
    pthread_t p;
    uint64_t t0 = now_ns(), next = 1, bad = 0;
    pthread_create(&p, NULL, producer, NULL);
    while (next <= ITEMS) {
        uint64_t v = __atomic_load_n(&slot, __ATOMIC_ACQUIRE);
        if (v == 0) { yields[1]++; sched_yield(); continue; }
        uint64_t expect = v;
        if (!__atomic_compare_exchange_n(&slot, &expect, 0, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            cas_fail[1]++;
            cas_spurious[1] += expect == v;
            continue;
        }
        bad += v != next++;
    }
    pthread_join(p, NULL);
    emit("mailbox_out_of_order: %llu", (unsigned long long)bad);
    emit("mailbox_cas_fail: %llu,%llu", (unsigned long long)cas_fail[0], (unsigned long long)cas_fail[1]);
    emit("mailbox_cas_spurious: %llu,%llu", (unsigned long long)cas_spurious[0], (unsigned long long)cas_spurious[1]);
    emit("mailbox_yields: %llu,%llu", (unsigned long long)yields[0], (unsigned long long)yields[1]);
    emit("mailbox_ns: %llu", (unsigned long long)(now_ns() - t0));
}

/* f. two threads on a pthread spinlock; digest of the owner sequence */
static pthread_spinlock_t lock;
static uint64_t owner_hash = 1469598103934665603u, acquisitions, switches;
static int last_owner = -1;
static void *contender(void *arg) {
    int id = (int)(uintptr_t)arg;
    for (int i = 0; i < 20000; i++) {
        pthread_spin_lock(&lock);
        owner_hash = (owner_hash ^ (uint64_t)(id + 1)) * 1099511628211u;
        switches += last_owner != id;
        last_owner = id;
        acquisitions++;
        pthread_spin_unlock(&lock);
        spin(5);
    }
    return NULL;
}
static void phase_spinlock(void) {
    pthread_t t[2];
    uint64_t t0 = now_ns();
    pthread_spin_init(&lock, PTHREAD_PROCESS_PRIVATE);
    for (uintptr_t i = 0; i < 2; i++) pthread_create(&t[i], NULL, contender, (void *)i);
    for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);
    emit("spin_acquisitions: %llu", (unsigned long long)acquisitions);
    emit("spin_owner_switches: %llu", (unsigned long long)switches);
    emit("spin_owner_digest: %016llx", (unsigned long long)owner_hash);
    emit("spin_ns: %llu", (unsigned long long)(now_ns() - t0));
}

/* g. self-modifying JIT: RW -> write -> RX -> call, 100 rewrites */
static void phase_jit(void) {
#if defined(__x86_64__) || defined(__aarch64__)
    uint64_t t0 = now_ns();
    int mismatches = 0;
    unsigned char *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) exit(2);
    for (uint32_t i = 0; i <= 100; i++) {
        uint32_t want = 1000 + 7 * i;
        mprotect(page, 4096, PROT_READ | PROT_WRITE);
#if defined(__x86_64__)
        page[0] = 0xb8; /* mov eax, imm32 */
        memcpy(page + 1, &want, 4);
        page[5] = 0xc3; /* ret */
#else
        uint32_t code[2] = {0x52800000u | (want << 5), 0xd65f03c0u}; /* movz w0, #want; ret */
        memcpy(page, code, sizeof code);
#endif
        mprotect(page, 4096, PROT_READ | PROT_EXEC);
        __builtin___clear_cache((char *)page, (char *)page + 16);
        int (*fn)(void);
        void *addr = page;
        memcpy(&fn, &addr, sizeof fn);
        mismatches += (uint32_t)fn() != want;
    }
    munmap(page, 4096);
    emit("jit_calls: 101");
    emit("jit_mismatches: %d", mismatches);
    emit("jit_ns: %llu", (unsigned long long)(now_ns() - t0));
#else
    emit("jit: unsupported");
#endif
}

/* h. 500 SIGUSR1 from a thread while main spins and sleeps */
static volatile sig_atomic_t handled;
static volatile int sender_done;
static pthread_t main_thread;
static void on_usr1(int sig) { (void)sig; handled++; }
static void *sender(void *arg) {
    (void)arg;
    for (int i = 0; i < 500; i++) {
        pthread_kill(main_thread, SIGUSR1);
        sched_yield();
    }
    __atomic_store_n(&sender_done, 1, __ATOMIC_RELEASE);
    return NULL;
}
static void phase_signals(void) {
    struct sigaction sa = {0};
    pthread_t t;
    uint64_t t0 = now_ns(), rounds = 0, eintr = 0;
    sa.sa_handler = on_usr1; /* no SA_RESTART: nanosleep returns EINTR */
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR1, &sa, NULL);
    main_thread = pthread_self();
    pthread_create(&t, NULL, sender, NULL);
    while (!__atomic_load_n(&sender_done, __ATOMIC_ACQUIRE)) {
        spin(1000);
        rounds++;
        struct timespec ts = {0, 50000};
        if (nanosleep(&ts, NULL) && errno == EINTR) eintr++;
    }
    pthread_join(t, NULL);
    emit("signal_handled: %d/500", (int)handled);
    emit("signal_eintr: %llu", (unsigned long long)eintr);
    emit("signal_spin_rounds: %llu", (unsigned long long)rounds);
    emit("signal_ns: %llu", (unsigned long long)(now_ns() - t0));
}

int main(void) {
    phase_pipelines();
    phase_spawn();
    phase_fork_compute();
    phase_threads();
    phase_mailbox();
    phase_spinlock();
    phase_jit();
    phase_signals();
    /* One more translated process tree: the digest of every line above. */
    FILE *sum = popen("printf 'digest: %s\\n' \"$(sha256sum | cut -d' ' -f1)\"", "w");
    if (!sum) return 2;
    fputs(trace, sum);
    return pclose(sum) == 0 ? 0 : 2;
}
