/* Ordinary Linux mapping/COW lifecycle with executed AArch64 code. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static size_t page;
static unsigned checks, failures;
static void publish(uint32_t *write, uint32_t *execute, unsigned value)
{
    write[0] = 0x52800000u | (value << 5);
    write[1] = 0xd65f03c0;
    __builtin___clear_cache((char *)write, (char *)(write + 2));
    if (write != execute)
        __builtin___clear_cache((char *)execute, (char *)(execute + 2));
}
static void expect(uint32_t *code, unsigned expected)
{
    unsigned actual = ((unsigned (*)(void))code)();
    ++checks;
    if (actual != expected) {
        ++failures;
        fprintf(stderr, "mapping check=%u expected=%u actual=%u\n", checks, expected, actual);
    }
}
static void require(int condition)
{
    if (!condition) {
        perror("ic-mappings setup");
        exit(2);
    }
}
int main(void)
{
    long size = sysconf(_SC_PAGESIZE);
    require(size > 0);
    page = (size_t)size;
    /* HotSpot probes madvise support with an empty range. No mapping changes. */
    require(madvise(NULL, 0, MADV_DONTNEED) == 0);
    uint32_t *code = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE | PROT_EXEC,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    require(code != MAP_FAILED);
    publish(code, code, 1); expect(code, 1);
    uint32_t *second = (uint32_t *)((char *)code + page);
    publish(second, second, 2); expect(second, 2);
    /* Partial permissions split the VMA and revoke its old source incarnation. */
    require(mprotect(second, page, PROT_READ) == 0);
    require(mprotect(second, page, PROT_READ | PROT_WRITE | PROT_EXEC) == 0);
    expect(code, 1); expect(second, 2);
    publish(second, second, 3); expect(second, 3);
    /* Reuse exactly the same address and bytes, then publish a changed body. */
    require(mmap(code, page, PROT_READ | PROT_WRITE | PROT_EXEC,
                 MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == code);
    publish(code, code, 1); expect(code, 1);
    publish(code, code, 4); expect(code, 4);
    require(madvise(code, page, MADV_DONTNEED) == 0);
    publish(code, code, 5); expect(code, 5);
    /* Ordinary kernel output into code is published by the same IC contract. */
    int pipefd[2];
    uint32_t replacement[2] = {0x52800160, 0xd65f03c0}; /* return 11 */
    require(pipe(pipefd) == 0);
    require(write(pipefd[1], replacement, sizeof(replacement)) == sizeof(replacement));
    require(read(pipefd[0], code, sizeof(replacement)) == sizeof(replacement));
    close(pipefd[0]); close(pipefd[1]);
    __builtin___clear_cache((char *)code, (char *)(code + 2));
    expect(code, 11);
    publish(code, code, 5); expect(code, 5);

    pid_t child = fork();
    require(child >= 0);
    if (child == 0) {
        unsigned prior = failures;
        expect(code, 5);
        publish(code, code, 6); expect(code, 6);
        _exit(failures != prior);
    }
    int status;
    require(waitpid(child, &status, 0) == child);
    require(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    ++checks; /* Independently checked child's two executed calls. */
    expect(code, 5); /* Child's COW write cannot change the parent. */

    uint32_t *moved = mremap(code, page, page, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    require(moved != MAP_FAILED);
    expect(moved, 5);
    publish(code, code, 7); expect(code, 7); expect(moved, 5);
    require(munmap(moved, page) == 0);
    require(munmap(code, 3 * page) == 0);

    int fd = memfd_create("ic-mappings", 0);
    require(fd >= 0 && ftruncate(fd, page) == 0);
    uint32_t *shared = mmap(NULL, page, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_SHARED, fd, 0);
    uint32_t *private = mmap(NULL, page, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE, fd, 0);
    require(shared != MAP_FAILED && private != MAP_FAILED);
    close(fd);
    publish(shared, private, 8); expect(shared, 8); expect(private, 8);
    publish(private, private, 9); /* Establish the actual COW page. */
    expect(private, 9); expect(shared, 8);
    publish(shared, shared, 10); expect(shared, 10); expect(private, 9);
    require(munmap(shared, page) == 0 && munmap(private, page) == 0);
    printf("ic-mappings: checks=%u failures=%u\n", checks, failures);
    return failures != 0;
}
