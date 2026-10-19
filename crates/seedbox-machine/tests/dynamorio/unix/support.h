/* Public Linux process/syscall helpers. No DR state is inspected here. */
#ifndef SEEDBOX_UNIX_SUPPORT_H
#define SEEDBOX_UNIX_SUPPORT_H
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>
/* A public application marker with an explicit continuation PC. The client
 * flushes from its clean call and redirects to this label as required by the
 * dr_flush_region contract, rather than returning into a possibly flushed BB. */
#define UNIX_SUSPEND_MARKER \
    void unix_request_suspend(void); \
    asm(".text\n.globl unix_request_suspend\n.type unix_request_suspend,%function\nunix_request_suspend:\nnop\n.globl unix_after_suspend\nunix_after_suspend:\nret\n")
static inline void unix_emit(uint32_t *p, unsigned value) {
#if defined(__aarch64__)
    p[0] = 0x52800000u | (value & 0xffff) << 5;
    p[1] = 0xd65f03c0u;
#else
    /* mov imm32,%eax; ret. The two aligned stores are not shared with executors. */
    unsigned char *b = (unsigned char *)p;
    b[0] = 0xb8; __builtin_memcpy(b + 1, &value, 4); b[5] = 0xc3;
#endif
    __builtin___clear_cache((char *)p, (char *)p + 8);
}
static inline void unix_park(void) {
    static int word;
    for (;;) syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
}
/* A positive public kernel observation, never a timed absence assumption.
 * The runner's HangGuard bounds a broken runtime or inaccessible procfs. */
static inline void unix_wait_syscall(pid_t pid, pid_t tid, long nr) {
    char path[96], text[512];
    snprintf(path, sizeof path, "/proc/%d/task/%d/syscall", pid, tid);
    for (;;) {
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) { perror(path); exit(2); }
        ssize_t n = read(fd, text, sizeof text - 1);
        close(fd);
        long got;
        if (n < 0) { perror(path); exit(2); }
        text[n] = 0;
        if (sscanf(text, "%ld", &got) == 1 && got == nr) return;
        sched_yield();
    }
}
#endif
