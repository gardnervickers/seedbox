/* Source: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/repros/sigext.c.
 * Readiness is acknowledged, pacing is finite work, and child ownership remains explicit. */
/*
 * Process-directed signals from another process while code is generated, as the
 * Signals probe does (a child runs `kill -HUP <jvm>`). The handler posts a
 * semaphore that a dispatcher thread waits on, like HotSpot's signal dispatcher.
 * Natively every signal arrives within the 5 s wait and it prints "OK".
 *
 * Usage: sigext ROUNDS [EXECUTORS [PARKED]]
 */
#define _GNU_SOURCE
#include "support.h"
#include <semaphore.h>
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static sem_t pending, dispatched, parked;
static atomic_int stop;
static uint32_t *code;

static void on_hup(int sig) {
    (void)sig;
    sem_post(&pending);
}

static void *dispatcher(void *arg) {
    (void)arg;
    while (!atomic_load(&stop)) {
        if (sem_wait(&pending) == 0) sem_post(&dispatched);
    }
    return NULL;
}

static void *writer(void *arg) {
    (void)arg;
    for (uint32_t i = 0; !atomic_load(&stop); i++) {
        uint32_t slot = (i % 64) * 4;
        unix_emit(&code[slot], slot);
        sched_yield();
    }
    return NULL;
}

static void *executor(void *arg) {
    for (uint32_t i = (uintptr_t)arg; !atomic_load(&stop); i++) {
        uint32_t slot = (i % 64) * 4;
        if (((int (*)(void)) & code[slot])() != (int)slot) {
            fprintf(stderr, "bad generated-code result\n");
            exit(3);
        }
    }
    return NULL;
}

static void *parker(void *arg) {
    (void)arg;
    sem_wait(&parked);
    return NULL;
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 50;
    int executors = argc > 2 ? atoi(argv[2]) : 4;
    int parkers = argc > 3 ? atoi(argv[3]) : 16;
    sem_init(&pending, 0, 0);
    sem_init(&dispatched, 0, 0);
    sem_init(&parked, 0, 0);
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_hup;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGHUP, &sa, NULL);
    code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (uint32_t slot = 0; slot < 256; slot += 4) {
        unix_emit(&code[slot], slot);
    }
    __builtin___clear_cache((char *)code, (char *)code + 4096);
    pthread_t t;
    pthread_create(&t, NULL, dispatcher, NULL);
    pthread_create(&t, NULL, writer, NULL);
    for (int i = 0; i < executors; i++) pthread_create(&t, NULL, executor, (void *)(uintptr_t)i);
    for (int i = 0; i < parkers; i++) pthread_create(&t, NULL, parker, NULL);
    char pid[32];
    snprintf(pid, sizeof(pid), "%d", (int)getpid());
    int lost = 0;
    for (int r = 0; r < rounds; r++) {
        pid_t child = fork();
        if (child == 0) {
            execl(UNIX_KILL_PATH, "kill", "-HUP", pid, (char *)NULL);
            _exit(127);
        }
        int status;
        waitpid(child, &status, 0);
        struct timespec deadline;
        clock_gettime(CLOCK_REALTIME, &deadline);
        deadline.tv_sec += 5;
        int rc;
        while ((rc = sem_timedwait(&dispatched, &deadline)) != 0 && errno == EINTR) {
        }
        if (rc != 0) {
            lost++;
            fprintf(stderr, "round %d: signal not dispatched within 5 s\n", r);
        }
    }
    printf(lost ? "BAD %d lost\n" : "OK\n", lost);
    fflush(stdout);
    _exit(lost != 0);
}
