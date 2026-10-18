/* mapfixed_stale MODE [N]: code at an address is replaced by a new mapping, without cache
 * maintenance by the application, and called again. The kernel makes a newly mapped page
 * coherent, so natively the call returns the new code's value. N rounds (default 100).
 *   file    MAP_FIXED of a different file (memfd) over executable anonymous memory
 *   revert  MADV_DONTNEED on a private file mapping whose code was overwritten (with cache
 *           maintenance) in the private copy: the file's code returns
 *   remap   munmap, then mmap of the file at the same address (control)
 * Prints "stale K of N" (natively 0) and "OK". */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static void tag(uint32_t *p, uint32_t v) { p[0] = 0x52800000u | (v & 0xffff) << 5; p[1] = 0x72a00000u | (v >> 16) << 5; p[2] = 0xd65f03c0u; }
typedef uint32_t (*fn_t)(void);
int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "file";
    int n = argc > 2 ? atoi(argv[2]) : 100, stale = 0;
    size_t pg = sysconf(_SC_PAGESIZE);
    uint32_t *buf = calloc(1, pg);
    int fd = memfd_create("code", 0);
    if (ftruncate(fd, pg)) return 2;
    uint32_t *p = mmap(NULL, pg, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (int i = 0; i < n; i++) {
        uint32_t a = 0x10000 + i, b = 0x20000 + i;
        /* A: anonymous (or private) code, written and published with cache maintenance. */
        if (!strcmp(mode, "revert")) {
            tag(buf, b);
            if (pwrite(fd, buf, pg, 0) != (ssize_t)pg) return 2;
            p = mmap(p, pg, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_FIXED, fd, 0);
        } else {
            p = mmap(p, pg, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
        }
        tag(p, a);
        __builtin___clear_cache((char *)p, (char *)p + 12);
        uint32_t got_a = ((fn_t)p)();
        /* B: the new code, through a new mapping only. */
        if (!strcmp(mode, "revert")) {
            madvise(p, pg, MADV_DONTNEED);
        } else {
            tag(buf, b);
            if (pwrite(fd, buf, pg, 0) != (ssize_t)pg) return 2;
            if (!strcmp(mode, "remap")) {
                munmap(p, pg);
                if (mmap(p, pg, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0) != p) return 3;
            } else {
                mmap(p, pg, PROT_READ | PROT_EXEC, MAP_SHARED | MAP_FIXED, fd, 0);
            }
        }
        asm volatile("isb" ::: "memory");
        uint32_t got_b = ((fn_t)p)();
        if (got_a != a || got_b != b) {
            if (stale < 3) fprintf(stderr, "round %d: first %#x (want %#x), after %s %#x (want %#x)\n", i, got_a, a, mode, got_b, b);
            stale++;
        }
    }
    printf("stale %d of %d\n%s\n", stale, n, stale ? "FAIL" : "OK");
    return stale != 0;
}
