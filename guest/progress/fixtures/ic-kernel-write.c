/* Linux's /proc/self/mem write path publishes executable bytes through
 * copy_to_user_page(), without an application IC IVAU. Keep this distinct from
 * a userspace store that omitted its required cache maintenance. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static volatile uint32_t external_done;

int main(int argc, char **argv)
{
    int external = argc == 2 && strcmp(argv[1], "--external") == 0;
    if (argc != 1 && !external)
        return 2;
    long page = sysconf(_SC_PAGESIZE);
    if (page <= 0)
        return 2;
    uint32_t *code = mmap(NULL, (size_t)page, PROT_READ | PROT_WRITE | PROT_EXEC,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED)
        return 2;
    code[0] = 0x52800020; /* mov w0,#1 */
    code[1] = 0xd65f03c0; /* ret */
    __builtin___clear_cache((char *)code, (char *)(code + 2));
    int (*body)(void) = (int (*)(void))code;
    int before = body();
    uint32_t replacement = 0x52800040; /* mov w0,#2 */
    if (external) {
        /* The ancestor observer writes both words through /proc/PID/mem.
         * No application syscall or IC occurs in this wait. */
        printf("ic-kernel-write-ready: pid=%ld code=%p done=%p\n", (long)getpid(),
               (void *)code, (void *)&external_done);
        fflush(stdout);
        while (!external_done)
            __asm__ volatile("" ::: "memory");
    } else {
        int fd = open("/proc/self/mem", O_RDWR | O_CLOEXEC);
        if (fd < 0)
            return 2;
        if (pwrite(fd, &replacement, sizeof(replacement), (off_t)(uintptr_t)code) !=
            sizeof(replacement))
            return 2;
        close(fd);
    }
    int after = body();
    int failed = before != 1 || after != 2 || code[0] != replacement;
    printf("ic-kernel-write: before=%d after=%d bytes=0x%08x failures=%d\n",
           before, after, code[0], failed);
    munmap(code, (size_t)page);
    return failed;
}
