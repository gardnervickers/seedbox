/* Rewrites and republishes code while other threads execute it, then races an
 * unsynchronized counter. AArch64 publishes with DC CVAU/IC IVAU
 * (__builtin___clear_cache); x86-64 relies on plain stores. Prints one line
 * per phase. The racy count depends on the guest schedule, so equal output
 * across runs is a determinism check, not a property of the program.
 * Usage: rewrite ROUNDS RACE_ITERATIONS
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define READERS 3
#define SLOTS 8

static unsigned char *code;
static pthread_barrier_t barrier;
static int rounds;
static volatile int generation;
static long failures[READERS];
static uint64_t sums[READERS];

static void emit(int slot, unsigned value)
{
    unsigned char *p = code + slot * 64;
#if defined(__aarch64__)
    uint32_t insn[2] = {0x52800000u | ((value & 65535u) << 5), 0xd65f03c0u}; /* movz w0; ret */
    memcpy(p, insn, sizeof insn);
    __builtin___clear_cache((char *)p, (char *)p + sizeof insn);
#elif defined(__x86_64__)
    p[0] = 0xb8; /* mov eax, imm32 */
    memcpy(p + 1, &value, 4);
    p[5] = 0xc3; /* ret */
#else
#error unsupported architecture
#endif
}

static unsigned call(int slot)
{
    return ((unsigned (*)(void))(code + slot * 64))();
}

static void *reader(void *arg)
{
    size_t id = (size_t)arg;
    for (int r = 1; r <= rounds; r++) {
        pthread_barrier_wait(&barrier);
        for (int s = 0; s < SLOTS; s++) {
            unsigned want = (unsigned)(r * 131 + s * 7) & 65535u;
            unsigned got = call(s);
            failures[id] += got != want;
            sums[id] = sums[id] * 1099511628211ull + got;
        }
        pthread_barrier_wait(&barrier);
    }
    return NULL;
}

static long counter;
static long iterations;
static void *racer(void *arg)
{
    (void)arg;
    for (long i = 0; i < iterations; i++) {
        long v = *(volatile long *)&counter;
        *(volatile long *)&counter = v + 1;
    }
    return NULL;
}

int main(int argc, char **argv)
{
    if (argc != 3) {
        fprintf(stderr, "usage: rewrite ROUNDS RACE_ITERATIONS\n");
        return 2;
    }
    rounds = atoi(argv[1]);
    iterations = atol(argv[2]);
    setvbuf(stdout, NULL, _IOLBF, 0);
    code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED)
        return 3;
    pthread_t threads[READERS];
    if (pthread_barrier_init(&barrier, NULL, READERS + 1))
        return 3;
    for (size_t i = 0; i < READERS; i++)
        if (pthread_create(&threads[i], NULL, reader, (void *)i))
            return 4;
    for (int r = 1; r <= rounds; r++) {
        for (int s = 0; s < SLOTS; s++)
            emit(s, (unsigned)(r * 131 + s * 7) & 65535u);
        generation = r;
        pthread_barrier_wait(&barrier);
        pthread_barrier_wait(&barrier);
    }
    long failed = 0;
    uint64_t sum = 0;
    for (int i = 0; i < READERS; i++) {
        pthread_join(threads[i], NULL);
        failed += failures[i];
        sum ^= sums[i];
    }
    printf("rewrite rounds=%d failures=%ld sum=%016llx\n", rounds, failed, (unsigned long long)sum);
    pthread_t race[2];
    for (int i = 0; i < 2; i++)
        if (pthread_create(&race[i], NULL, racer, NULL))
            return 4;
    for (int i = 0; i < 2; i++)
        pthread_join(race[i], NULL);
    printf("race counter=%ld expected=%ld\n", counter, 2 * iterations);
    return failed != 0;
}
