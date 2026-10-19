#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <errno.h>
#include <unistd.h>
#include <sys/syscall.h>
static sem_t ready, ack;
static _Atomic unsigned stop_loop;
static _Thread_local volatile sig_atomic_t nested;
static void fail(void) { _exit(2); }
static void receive(int sig) {
    if (sig == SIGUSR2) { nested++; return; }
    sig_atomic_t before = nested;
    if (raise(SIGUSR2) != 0) fail();
    if (nested != before + 1) fail();
    if (sem_post(&ack) != 0) fail();
}
__attribute__((noinline,visibility("default"))) void shared_loop(void) {
    while (!atomic_load_explicit(&stop_loop, memory_order_relaxed)) {
#ifdef LOOP_SYSCALL
# if defined(__x86_64__)
        long result;
        __asm__ volatile("syscall" : "=a"(result) : "a"(SYS_sched_yield)
                         : "rcx", "r11", "memory");
# elif defined(__aarch64__)
        register long number __asm__("x8") = SYS_sched_yield;
        register long result __asm__("x0");
        __asm__ volatile("svc #0" : "=r"(result) : "r"(number) : "memory", "cc");
# else
#  error unsupported syscall control architecture
# endif
        if (result != 0) fail();
#endif
        __asm__ volatile("" ::: "memory");
    }
}
__attribute__((noinline,visibility("default"))) void flush_marker(void) {
    __asm__ volatile("" ::: "memory");
}
static void wait_event(sem_t *s) {
    while (sem_wait(s) != 0) if (errno != EINTR) fail();
}
static void *worker(void *unused) {
    if (sem_post(&ready) != 0) fail();
    shared_loop();
    return NULL;
}
int main(void) {
    struct sigaction sa = {.sa_handler=receive};
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGUSR1,&sa,NULL) || sigaction(SIGUSR2,&sa,NULL)) fail();
    if (sem_init(&ready,0,0) || sem_init(&ack,0,0)) fail();
    pthread_t threads[2];
    for (unsigned i=0;i<2;i++) if (pthread_create(&threads[i],NULL,worker,NULL)) fail();
    wait_event(&ready); wait_event(&ready);
    for (unsigned round=0;round<64;round++) {
        for(unsigned i=0;i<2;i++) if(pthread_kill(threads[i],SIGUSR1)) fail();
        wait_event(&ack); wait_event(&ack);
        flush_marker();
    }
    atomic_store_explicit(&stop_loop,1,memory_order_relaxed);
    for(unsigned i=0;i<2;i++) if(pthread_join(threads[i],NULL)) fail();
    puts("handoff: threads=2 acks=128 nested=128 rounds=64");
    return 0;
}
