/* AArch64 instruction-cache regression. Three reader threads repeatedly execute
 * a patched function, a block crossing a page boundary patched only on its second
 * page, and an untouched neighboring function. All writes happen between barriers.
 * Use --skip-flush with -no_hw_cache_consistency as a cache-maintenance negative
 * control; hardware cache consistency can detect writes without explicit flushes.
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static uint32_t *code;
static size_t page;
static pthread_barrier_t barrier;
static int iteration;
static int skip_flush;
static int failures[3];
static int call(size_t offset)
{
    return ((int (*)(void))((char *)code + offset))();
}

static void write_return(size_t offset, unsigned value)
{
    uint32_t *p = (uint32_t *)((char *)code + offset);
    p[0] = 0x52800000u | ((value & 65535u) << 5); /* movz w0, value */
    p[1] = 0xd65f03c0u; /* ret */
    if (!skip_flush)
        __builtin___clear_cache((char *)p, (char *)(p + 2));
}

static void write_cross(unsigned value)
{
    uint32_t *p = (uint32_t *)((char *)code + page);
    p[0] = 0x11000000u | ((value & 4095u) << 10); /* add w0, w0, value */
    if (!skip_flush)
        __builtin___clear_cache((char *)p, (char *)(p + 1));
}

static void *reader(void *arg)
{
    size_t id = (size_t)arg;
    for (int i = 1; i <= 1000; i++) {
        pthread_barrier_wait(&barrier);
        failures[id] += call(0) != iteration;
        failures[id] += call(page - 4) != iteration + 1;
        failures[id] += call(2 * page) != 123;
        pthread_barrier_wait(&barrier);
    }
    return NULL;
}
int main(int argc, char **argv)
{
    if (argc > 2 || (argc == 2 && strcmp(argv[1], "--skip-flush") != 0)) {
        fprintf(stderr, "usage: jit-pages [--skip-flush]\n");
        return 2;
    }
    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0)
        return 2;
    page = (size_t)page_size;
    code = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED)
        return 2;
    write_return(0, 1);
    write_return(page - 4, 0);
    ((uint32_t *)((char *)code + page))[1] = 0xd65f03c0u;
    __builtin___clear_cache((char *)code + page - 4, (char *)code + page + 8);
    write_cross(2); /* Patch only the second page of a cross-page block. */
    write_return(2 * page, 123); /* Must survive unrelated patching. */
    skip_flush = argc > 1;
    pthread_t threads[3];
    if (pthread_barrier_init(&barrier, NULL, 4))
        return 3;
    for (size_t i = 0; i < 3; i++)
        if (pthread_create(&threads[i], NULL, reader, (void *)i))
            return 4;
    for (iteration = 1; iteration <= 1000; iteration++) {
        write_return(0, iteration);
        write_cross(iteration + 1);
        pthread_barrier_wait(&barrier);
        pthread_barrier_wait(&barrier);
    }
    int failed = 0;
    for (int i = 0; i < 3; i++) {
        pthread_join(threads[i], NULL);
        failed += failures[i];
    }
    printf("jit-pages: checks=9000 failures=%d\n", failed);
    return failed != 0;
}
