/* A waiter that spins on WFE and WFI until another thread sets a flag after a
 * virtual-time sleep. A host interruption wakes both hints, so without the
 * client's NOP rewrite the iteration count, and the progress charged for it,
 * would depend on host timing. The acceptance run compares full state across
 * two runs and a replay; this program only has to finish and agree. */
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>

static _Atomic int flag;

static void *waiter(void *arg)
{
    uint64_t spins = 0;
    for (;;) {
        int seen;
        __asm__ volatile("ldar %w0, [%1]\n\t"
                         "cbnz %w0, 1f\n\t"
                         "wfe\n\t"
                         "wfi\n\t"
                         "1:"
                         : "=r"(seen)
                         : "r"(&flag)
                         : "memory");
        if (seen)
            break;
        ++spins;
    }
    *(uint64_t *)arg = spins;
    return NULL;
}

int main(void)
{
    uint64_t spins = 0;
    pthread_t thread;
    if (pthread_create(&thread, NULL, waiter, &spins) != 0) {
        perror("pthread_create");
        return 1;
    }
    struct timespec delay = {.tv_sec = 0, .tv_nsec = 20000000};
    nanosleep(&delay, NULL);
    atomic_store(&flag, 1);
    __asm__ volatile("sev" ::: "memory");
    pthread_join(thread, NULL);
    printf("wait hints: %llu spins\n", (unsigned long long)spins);
    return 0;
}
