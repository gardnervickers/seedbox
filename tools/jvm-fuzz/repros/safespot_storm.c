/* safespot_storm [SECONDS]: writers rewrite movz;ret slots and flush them,
 * executors call them while receiving SIGUSR1 every ~20us, and a mapper maps,
 * runs and unmaps fresh RWX pages. Prints OK when it finishes. */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
static uint32_t *code;
static volatile int stop;
static volatile pid_t exec_tid[2];
static volatile long sigs;
static void h(int s) { (void)s; sigs++; }
static void put(uint32_t *slot, unsigned v) {
    slot[0] = 0x52800000 | ((v & 0xffff) << 5);
    slot[1] = 0xd65f03c0;
    __builtin___clear_cache((char *)slot, (char *)slot + 8);
}
static void *writer(void *p) {
    long k = (long)p;
    for (unsigned i = 0; !stop; i++) put(code + ((i * 2 + k) % 64) * 4, i);
    return NULL;
}
static void *executor(void *p) {
    long k = (long)p;
    exec_tid[k] = (pid_t)syscall(SYS_gettid);
    for (unsigned i = 0; !stop; i++) ((int (*)(void))(code + (i % 64) * 4))();
    return NULL;
}
static void *signaler(void *p) {
    (void)p;
    struct timespec ts = {0, 20000};
    for (unsigned i = 0; !stop; i++) {
        pid_t t = exec_tid[i & 1];
        if (t) syscall(SYS_tgkill, getpid(), t, SIGUSR1);
        nanosleep(&ts, NULL);
    }
    return NULL;
}
static void *mapper(void *p) {
    (void)p;
    for (unsigned i = 0; !stop; i++) {
        uint32_t *m = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        put(m, i);
        ((int (*)(void))m)();
        munmap(m, 4096);
    }
    return NULL;
}
int main(int argc, char **argv) {
    double secs = argc > 1 ? atof(argv[1]) : 2;
    struct sigaction a = {0}; a.sa_handler = h; a.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &a, NULL);
    code = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (int i = 0; i < 64; i++) put(code + i * 4, i);
    pthread_t t[6];
    pthread_create(&t[0], NULL, writer, (void *)0);
    pthread_create(&t[1], NULL, writer, (void *)1);
    pthread_create(&t[2], NULL, executor, (void *)0);
    pthread_create(&t[3], NULL, executor, (void *)1);
    pthread_create(&t[4], NULL, signaler, NULL);
    pthread_create(&t[5], NULL, mapper, NULL);
    struct timespec ts = {(time_t)secs, (long)((secs - (time_t)secs) * 1e9)};
    nanosleep(&ts, NULL);
    stop = 1;
    for (int i = 0; i < 6; i++) pthread_join(t[i], NULL);
    printf("OK\n");
    return 0;
}
