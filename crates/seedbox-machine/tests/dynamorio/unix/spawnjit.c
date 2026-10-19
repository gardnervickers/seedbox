/* Source: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/repros/spawnjit.c.
 * Readiness is acknowledged, pacing is finite work, and child ownership remains explicit. */
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
#include "support.h"
#include <semaphore.h>
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
static sem_t parked, ready;
static atomic_int writer_ready;

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
        unix_emit(&code[slot], i & 0xffff);
        int (*fn)(void) = (int (*)(void))&code[slot];
        if (fn() != (int)(i & 0xffff)) {
            fprintf(stderr, "bad generated-code result\n");
            exit(3);
        }
        if (atomic_fetch_add(&work, 1) == 0) sem_post(&ready);
    }
    return NULL;
}

static uint32_t *shared_code;

/* Patches slots of the shared page while executor threads call them. */
static void *writer(void *arg) {
    (void)arg;
    for (uint32_t i = 0; !atomic_load(&stop); i++) {
        uint32_t slot = (i % 64) * 4;
        unix_emit(&shared_code[slot], slot);
        atomic_store(&writer_ready, 1);
        sched_yield();
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
        if (atomic_fetch_add(&work, 1) == 0) sem_post(&ready);
    }
    return NULL;
}

static void *busy(void *arg) {
    volatile uint64_t x = (uintptr_t)arg;
    while (!atomic_load(&stop)) {
        x = x * 6364136223846793005ULL + 1;
        if (atomic_fetch_add(&work, 1) == 0) sem_post(&ready);
    }
    return NULL;
}

static void *sleeper(void *arg) {
    (void)arg; sem_post(&ready);
    while (sem_wait(&parked) && errno == EINTR) {}
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 4 || argc > 6) {
        fprintf(stderr, "usage: spawnjit SPAWNS THREAD MODE [COPIES [SLEEPERS]]\n");
        return 2;
    }
    sem_init(&parked, 0, 0); sem_init(&ready, 0, 0);
    int spawns = atoi(argv[1]), thread = atoi(argv[2]);
    const char *mode = argv[3];
    int copies = argc > 4 ? atoi(argv[4]) : 1, sleepers = argc > 5 ? atoi(argv[5]) : 0;
    if (!thread) copies = 0;
    pthread_t ts[copies + sleepers + 1];
    if (thread == 4) {
        shared_code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        for (uint32_t slot = 0; slot < 256; slot += 4) {
            unix_emit(&shared_code[slot], slot);
        }
        __builtin___clear_cache((char *)shared_code, (char *)shared_code + 4096);
        for (int k = 0; k < copies; k++) pthread_create(&ts[k], NULL, executor, (void *)(uintptr_t)k);
        pthread_create(&ts[copies + sleepers], NULL, writer, NULL);
    } else {
        for (int k = 0; k < copies; k++) pthread_create(&ts[k], NULL, thread == 1 ? jit : thread == 2 ? busy : sleeper, NULL);
    }
    for (int k = 0; k < sleepers; k++) pthread_create(&ts[copies + k], NULL, sleeper, NULL);
    if (thread==3) for(int k=1;k<copies;k++) while (sem_wait(&ready) && errno == EINTR) {}
    if (copies) while (sem_wait(&ready) && errno == EINTR) {}
    for (int k=0;k<sleepers;k++) while (sem_wait(&ready) && errno == EINTR) {}
    if(thread==4)while(!atomic_load(&writer_ready))sched_yield();
    double t0 = now();
    int bad = 0;
    for (int i = 0; i < spawns; i++) {
        char *child[] = {UNIX_SH_PATH, "-c", "exit 3", NULL};
        pid_t pid;
        if (!strcmp(mode, "spawn")) {
            if (posix_spawn(&pid, UNIX_SH_PATH, NULL, NULL, child, environ)) {
                perror("posix_spawn");
                return 2;
            }
        } else {
            pid = !strcmp(mode, "vfork") ? vfork() : fork();
            if (pid == 0) {
                execve(UNIX_SH_PATH, child, environ);
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
    for (int k=0;k<sleepers + (thread==3 ? copies : 0);k++) sem_post(&parked);
    for (int k = 0; k < copies + sleepers; k++) pthread_join(ts[k], NULL);
    if (thread == 4) pthread_join(ts[copies + sleepers], NULL);
    if (bad) printf("BAD %d\n", bad);
    else printf("OK\n");
    return bad != 0;
}
