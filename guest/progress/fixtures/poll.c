/* Owner/waiter spinlock whose waiter polls with an exclusive load and no
 * pause, system call or clock read. Both shapes must keep making progress
 * under translation. PACKED permits legacy DynamoRIO sequence packing;
 * otherwise an adr inside the sequence prevents packing. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static unsigned lock = 1;
static unsigned long polls;

static void *waiter(void *arg)
{
    (void)arg;
    unsigned value, failed, one, tries = 0;
    __asm__ volatile(
        "0:\n\t"
        "ldaxr %w0, [%4]\n\t"
#ifndef PACKED
        "adr x9, 0b\n\t"
#endif
        "add %w3, %w3, #1\n\t"
        "cbnz %w0, 0b\n\t"
        "mov %w2, #1\n\t"
        "stxr %w1, %w2, [%4]\n\t"
        "cbnz %w1, 0b"
        : "=&r"(value), "=&r"(failed), "=&r"(one), "+r"(tries)
        : "r"(&lock)
        : "x9", "memory");
    polls = tries;
    return NULL;
}

int main(int argc, char **argv)
{
    long hold_ms = argc > 1 ? atol(argv[1]) : 200;
    pthread_t thread;
    if (pthread_create(&thread, NULL, waiter, NULL)) return 2;
    struct timespec hold = {hold_ms / 1000, (hold_ms % 1000) * 1000000L};
    nanosleep(&hold, NULL);
    __atomic_store_n(&lock, 0, __ATOMIC_RELEASE);
    pthread_join(thread, NULL);
    printf("POLL acquired=%u polls=%lu\n", lock, polls);
    return lock == 1 ? 0 : 1;
}
