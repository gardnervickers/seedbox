/* Uninstrumented application fixture. No PAUSE, clock read, syscall or yield in
 * spin_wait. The owner sleeps while holding the lock, then releases it.
 * The changing spin count prevents treating the waiter as a recurring idle cycle.
 */
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <errno.h>

static atomic_int held = 1;
static pthread_barrier_t ready;
static uint64_t sleep_ns;

static uint64_t now_ns(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return 0;
    return (uint64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}
static void *owner(void *unused) {
    pthread_barrier_wait(&ready);
    uint64_t start = now_ns();
    struct timespec delay = {0, 5000000};
    while (nanosleep(&delay, &delay) && errno == EINTR) {}
    sleep_ns = now_ns() - start;
    atomic_store_explicit(&held, 0, memory_order_release);
    return unused;
}
__attribute__((noinline)) static uint64_t spin_wait(void) {
    volatile uint64_t spins = 0;
    while (atomic_load_explicit(&held, memory_order_acquire)) ++spins;
    return spins;
}
int main(void) {
    pthread_t thread;
    if (pthread_barrier_init(&ready, NULL, 2) ||
        pthread_create(&thread, NULL, owner, NULL)) return 2;
    pthread_barrier_wait(&ready);
    puts("waiter: spinning while owner must wake and release lock");
    fflush(stdout);
    uint64_t spins = spin_wait();
    if (pthread_join(thread, NULL)) return 2;
    printf("released: spins=%llu sleep_ns=%llu\n",
           (unsigned long long)spins, (unsigned long long)sleep_ns);
    return spins > 0 && sleep_ns >= 5000000 ? 0 : 1;
}
