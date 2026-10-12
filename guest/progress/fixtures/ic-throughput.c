/* Fixed executed useful work for corrected-DR versus indexed-DR timings.
 * Both the publisher and every generated body run through the same client. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    if (argc != 3 || (strcmp(argv[1], "append") && strcmp(argv[1], "patch")))
        return 2;
    char *end;
    unsigned long count = strtoul(argv[2], &end, 10);
    if (*end || count == 0 || count > 60000)
        return 2;
    int append = strcmp(argv[1], "append") == 0;
    size_t size = (count + 1) * 16;
    uint32_t *code = mmap(NULL, size, PROT_READ | PROT_WRITE | PROT_EXEC,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED)
        return 2;
    code[0] = 0x528009a0; /* mov w0,#77: live untouched neighbor */
    code[1] = 0xd65f03c0;
    __builtin___clear_cache((char *)code, (char *)(code + 2));
    syscall(SYS_getpid, 0x53494344UL, code, size, 0);
    unsigned (*neighbor)(void) = (unsigned (*)(void))code;
    if (neighbor() != 77)
        return 1;
    syscall(SYS_getpid, 0x53494344UL, code, size, 1);
    uint64_t checksum = 0;
    for (unsigned long i = 1; i <= count; ++i) {
        uint32_t *body = code + (append ? i : 1) * 4;
        body[0] = 0x52800000u | ((unsigned)i << 5);
        body[1] = 0xd65f03c0;
        __builtin___clear_cache((char *)body, (char *)(body + 2));
        unsigned (*current)(void) = (unsigned (*)(void))body;
        for (unsigned j = 0; j < 128; ++j) {
            unsigned a = current(), b = neighbor();
            if (a != i || b != 77)
                return 1;
            checksum += a + b;
        }
    }
    uint64_t expected = 128 * ((uint64_t)count * (count + 1) / 2 + 77 * count);
    printf("ic-throughput: mode=%s publications=%lu calls=%lu checksum=%llu expected=%llu\n",
           argv[1], count, count * 256, (unsigned long long)checksum,
           (unsigned long long)expected);
    munmap(code, size);
    return checksum != expected;
}
