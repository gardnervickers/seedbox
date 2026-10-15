/* mjit THREADS ITERS: each thread rewrites "movz w0,#i; ret" slots in its own RWX page, flushes, calls. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
static int iters;
static void *run(void *arg) {
    long id = (long)arg;
    uint32_t *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    fprintf(stderr, "thread %ld page %p\n", id, (void *)code);
    for (uint32_t i = 0; i < (uint32_t)iters; i++) {
        uint32_t slot = (i % 64) * 4;
        code[slot] = 0x52800000u | ((i & 0xffff) << 5);
        code[slot + 1] = 0xd65f03c0u;
        __builtin___clear_cache((char *)&code[slot], (char *)&code[slot + 2]);
        int got = ((int (*)(void))&code[slot])();
        if (got != (int)(i & 0xffff)) {
            fprintf(stderr, "BAD thread %ld iter %u slot %u addr %p got %d\n", id, i, slot, (void *)&code[slot], got);
            exit(3);
        }
    }
    return NULL;
}
int main(int argc, char **argv) {
    int n = atoi(argv[1]); iters = atoi(argv[2]);
    pthread_t t[64];
    for (long k = 0; k < n; k++) pthread_create(&t[k], NULL, run, (void *)k);
    for (int k = 0; k < n; k++) pthread_join(t[k], NULL);
    puts("OK");
    return 0;
}
