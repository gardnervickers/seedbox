/*
 * Child-process creation while another thread writes and runs generated code,
 * as a JVM does during Process.start(). Natively every spawn returns exit 3.
 *
 * Usage: spawnjit SPAWNS THREAD MODE [COPIES [SLEEPERS]]
 *   THREAD:   0 none, 1 writes and runs code, 2 busy loop without code writes, 3 sleeps in a loop,
 *             4 one writer patches code that COPIES other threads keep calling (like a JIT)
 *   MODE:     spawn (posix_spawn), vfork, fork
 *   COPIES:   how many THREAD threads to start (default 1)
 *   SLEEPERS: extra idle threads, like a JVM's parked helpers (default 0)
 * Prints "OK", or "BAD n" when n children did not exit with status 3.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;
static atomic_int stop;
static atomic_long work;

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* Writes "movz w0, #imm; ret" into an executable page, flushes the icache, calls it; repeats. */
static void *jit(void *arg) {
    (void)arg;
    uint32_t *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED) {
        perror("mmap");
        exit(2);
    }
    for (uint32_t i = 0; !atomic_load(&stop); i++) {
        uint32_t slot = (i % 64) * 4;
        code[slot] = 0x52800000u | ((i & 0xffff) << 5);
        code[slot + 1] = 0xd65f03c0u;
        __builtin___clear_cache((char *)&code[slot], (char *)&code[slot + 2]);
        int (*fn)(void) = (int (*)(void))&code[slot];
        if (fn() != (int)(i & 0xffff)) {
            fprintf(stderr, "bad generated-code result\n");
            exit(3);
        }
        atomic_fetch_add(&work, 1);
    }
    return NULL;
}

static uint32_t *shared_code;

/* Patches slots of the shared page while executor threads call them. */
static void *writer(void *arg) {
    (void)arg;
    for (uint32_t i = 0; !atomic_load(&stop); i++) {
        uint32_t slot = (i % 64) * 4;
        shared_code[slot] = 0x52800000u | ((slot & 0xffff) << 5);
        shared_code[slot + 1] = 0xd65f03c0u;
        __builtin___clear_cache((char *)&shared_code[slot], (char *)&shared_code[slot + 2]);
        struct timespec ts = {0, 200000};
        nanosleep(&ts, NULL);
    }
    return NULL;
}

static void *executor(void *arg) {
    uint32_t n = (uintptr_t)arg;
    for (uint32_t i = n; !atomic_load(&stop); i++) {
        uint32_t slot = (i % 64) * 4;
        int (*fn)(void) = (int (*)(void))&shared_code[slot];
        if (fn() != (int)slot) {
            fprintf(stderr, "bad shared-code result\n");
            exit(3);
        }
        atomic_fetch_add(&work, 1);
    }
    return NULL;
}

static void *busy(void *arg) {
    volatile uint64_t x = (uintptr_t)arg;
    while (!atomic_load(&stop)) {
        x = x * 6364136223846793005ULL + 1;
        atomic_fetch_add(&work, 1);
    }
    return NULL;
}

static void *sleeper(void *arg) {
    (void)arg;
    struct timespec ts = {0, 1000000};
    while (!atomic_load(&stop)) {
        nanosleep(&ts, NULL);
        atomic_fetch_add(&work, 1);
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 4 || argc > 6) {
        fprintf(stderr, "usage: spawnjit SPAWNS THREAD MODE [COPIES [SLEEPERS]]\n");
        return 2;
    }
    int spawns = atoi(argv[1]), thread = atoi(argv[2]);
    const char *mode = argv[3];
    int copies = argc > 4 ? atoi(argv[4]) : 1, sleepers = argc > 5 ? atoi(argv[5]) : 0;
    if (!thread) copies = 0;
    pthread_t ts[copies + sleepers + 1];
    if (thread == 4) {
        shared_code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        for (uint32_t slot = 0; slot < 256; slot += 4) {
            shared_code[slot] = 0x52800000u | (slot << 5);
            shared_code[slot + 1] = 0xd65f03c0u;
        }
        __builtin___clear_cache((char *)shared_code, (char *)shared_code + 4096);
        for (int k = 0; k < copies; k++) pthread_create(&ts[k], NULL, executor, (void *)(uintptr_t)k);
        pthread_create(&ts[copies + sleepers], NULL, writer, NULL);
    } else {
        for (int k = 0; k < copies; k++) pthread_create(&ts[k], NULL, thread == 1 ? jit : thread == 2 ? busy : sleeper, NULL);
    }
    for (int k = 0; k < sleepers; k++) pthread_create(&ts[copies + k], NULL, sleeper, NULL);
    double t0 = now();
    int bad = 0;
    for (int i = 0; i < spawns; i++) {
        char *child[] = {"/bin/sh", "-c", "exit 3", NULL};
        pid_t pid;
        if (!strcmp(mode, "spawn")) {
            if (posix_spawn(&pid, "/bin/sh", NULL, NULL, child, environ)) {
                perror("posix_spawn");
                return 2;
            }
        } else {
            pid = !strcmp(mode, "vfork") ? vfork() : fork();
            if (pid == 0) {
                execve("/bin/sh", child, environ);
                _exit(127);
            }
        }
        int status = 0;
        if (waitpid(pid, &status, 0) != pid) {
            bad++;
            fprintf(stderr, "spawn %d: waitpid(%d = %#x) failed: %s (expected exit 3)\n", i, (int)pid, (unsigned)pid, strerror(errno));
        } else if (!WIFEXITED(status) || WEXITSTATUS(status) != 3) {
            bad++;
            fprintf(stderr, "spawn %d: child status %#x (expected exit 3)\n", i, status);
        }
        fprintf(stderr, "spawn %d done %.2fs work %ld\n", i, now() - t0, atomic_load(&work));
    }
    atomic_store(&stop, 1);
    for (int k = 0; k < copies + sleepers; k++) pthread_join(ts[k], NULL);
    if (thread == 4) pthread_join(ts[copies + sleepers], NULL);
    if (bad) printf("BAD %d\n", bad);
    else printf("OK\n");
    return 0;
}
