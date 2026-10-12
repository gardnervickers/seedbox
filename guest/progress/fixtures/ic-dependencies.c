/* Executed source-dependency regression. The observer's getpid markers carry
 * addresses only for diagnostics; publication and execution need no client API.
 * All writes use the shared RW alias. Each published version actually executes. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static uint32_t *rw;
static char *rx;
static size_t page;
static unsigned checks, failures;
static int writer_only;

static void phase(unsigned n)
{
    syscall(SYS_getpid, 0x53494344UL, rx, 2 * page, n);
}
static void publish(size_t offset, size_t size)
{
    __builtin___clear_cache((char *)rw + offset, (char *)rw + offset + size);
    if (!writer_only && rx != (char *)rw)
        __builtin___clear_cache(rx + offset, rx + offset + size);
}
static uint32_t mov(unsigned value) { return 0x52800000u | (value << 5); }
static uint32_t add(unsigned value) { return 0x11000000u | (value << 10); }
static void expect(size_t offset, int argument, int expected)
{
    int actual = ((int (*)(int))(rx + offset))(argument);
    ++checks;
    if (actual != expected) {
        ++failures;
        fprintf(stderr, "offset=%zu expected=%d actual=%d\n", offset, expected, actual);
    }
}
int main(int argc, char **argv)
{
    int anonymous = argc == 2 && strcmp(argv[1], "--anonymous") == 0;
    writer_only = argc == 2 && strcmp(argv[1], "--writer-only") == 0;
    if (argc != 1 && !anonymous && !writer_only) return 2;
    long size = sysconf(_SC_PAGESIZE);
    if (size <= 0) return 2;
    page = (size_t)size;
    if (anonymous) {
        rw = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE | PROT_EXEC,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        rx = (char *)rw;
    } else {
        int fd = memfd_create("ic-dependencies", 0);
        if (fd < 0 || ftruncate(fd, 2 * page)) return 2;
        rw = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        rx = mmap(NULL, 2 * page, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);
        close(fd);
    }
    if (rw == MAP_FAILED || rx == MAP_FAILED) return 2;
    rw[0] = mov(1); rw[1] = add(2); rw[2] = 0xd65f03c0;
    rw[8] = mov(77); rw[9] = 0xd65f03c0;
    rw[32] = 0x14000010; /* B +64: followed/elided, discontiguous dependency. */
    rw[48] = mov(30); rw[49] = 0xd65f03c0;
    rw[page / 4 - 1] = mov(4);
    rw[page / 4] = add(3); rw[page / 4 + 1] = 0xd65f03c0;
    publish(0, 200);
    publish(page - 4, 12);
    phase(0);
    expect(0, 10, 3); expect(4, 10, 12); expect(32, 0, 77);
    expect(128, 10, 30); expect(page - 4, 0, 7);

    phase(1); /* Same-line append and no-op publication retain all old entries. */
    rw[4] = mov(9); rw[5] = 0xd65f03c0;
    publish(16, 8);
    expect(16, 0, 9); expect(0, 0, 3); expect(32, 0, 77);
    publish(16, 8);
    expect(16, 0, 9); expect(32, 0, 77);

    phase(2); /* Two overlapping entries change; same-line neighbor does not. */
    rw[1] = add(5);
    publish(4, 4);
    expect(0, 0, 6); expect(4, 10, 15); expect(32, 0, 77);

    phase(3); /* Change an elided instruction, not the followed target. */
    rw[32] = 0x14000011;
    publish(128, 4);
    expect(128, 10, 10); expect(32, 0, 77);

    phase(4); /* Only second page published, entry lies in the first page. */
    rw[page / 4] = add(9);
    publish(page, 4);
    expect(page - 4, 0, 13); expect(32, 0, 77);

    phase(5); /* Equal bytes do not preserve a revoked executable incarnation. */
    if (mprotect(rx, 2 * page, PROT_READ) ||
        mprotect(rx, 2 * page, PROT_READ | PROT_EXEC)) return 2;
    publish(32, 8);
    expect(32, 0, 77);
    phase(6);
    printf("ic-dependencies: checks=%u failures=%u\n", checks, failures);
    munmap(rw, 2 * page);
    if (rx != (char *)rw) munmap(rx, 2 * page);
    return failures != 0;
}
