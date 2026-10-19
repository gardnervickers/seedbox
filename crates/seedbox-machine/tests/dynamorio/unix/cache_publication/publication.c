/* Temporary public Linux/AArch64 diagnostic, not maintained coverage. */
#define _GNU_SOURCE
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <pthread.h>
#include <sys/mman.h>
#include <unistd.h>
struct sync_state { _Atomic unsigned ready, done, entered; };
_Static_assert(sizeof(_Atomic unsigned) == 4, "assembly uses 32-bit events");
static struct sync_state state;
static uint32_t *code;
extern void publish_x0(uint32_t *, struct sync_state *);
extern void publish_x28(uint32_t *, struct sync_state *);
extern void publish_x30(uint32_t *, struct sync_state *);
static void (*publish)(uint32_t *, struct sync_state *);
__attribute__((noinline)) static int caller(void) { return ((int (*)(void))code)(); }
static void *writer(void *unused) { (void)unused; publish(code, &state); return NULL; }
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int reg = atoi(argv[1]);
    if (reg != 0 && reg != 28 && reg != 30) return 2;
    publish = reg == 0 ? publish_x0 : reg == 28 ? publish_x28 : publish_x30;
    long pagesize = sysconf(_SC_PAGESIZE);
    if (pagesize <= 0) return 2;
    code = mmap(NULL, pagesize, PROT_READ|PROT_WRITE|PROT_EXEC,
                MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED) return 2;
    code[0] = 0x52800020; /* mov w0, #1 */
    code[1] = 0xd65f03c0; /* ret */
    __builtin___clear_cache((char *)code, (char *)(code + 2));
    if (caller() != 1 || caller() != 1) return 1;
    pthread_t thread;
    if (pthread_create(&thread, NULL, writer, NULL)) return 2;
    int failed = 0;
    for (unsigned phase = 1; phase <= 2; phase++) {
        while (atomic_load_explicit(&state.ready, memory_order_acquire) != phase) {}
        /* Positive receipt from inside the writer's post-publication spin loop. */
        while (atomic_load_explicit(&state.entered, memory_order_acquire) != phase) {}
        __asm__ volatile("isb" ::: "memory");
        int got = caller();
        if (got != (int)phase) failed = 1;
        atomic_store_explicit(&state.done, phase, memory_order_release);
        printf("phase=%u value=%d expected=%u operand=x%d\n", phase, got, phase, reg);
    }
    if (pthread_join(thread, NULL)) return 2;
    munmap(code, pagesize);
    return failed;
}
