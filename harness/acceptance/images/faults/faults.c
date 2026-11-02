/* Guest-side observer of disk and scheduler faults on a named volume.
 *
 *   prepare FILE           write 8 blocks of a known pattern and fsync
 *   read FILE N            O_DIRECT read of N blocks: READ ok=A eio=B corrupt=C
 *   write FILE N           O_DIRECT overwrite of N blocks, each fdatasync'd:
 *                          WRITE ok=A eio=B
 *   fsync FILE N           N small appends each fsync'd, timed in guest
 *                          monotonic time: FSYNC min_ns=T max_ns=T
 *   race N                 two threads log themselves N times each with a
 *                          getppid syscall per entry: RACE switches=S
 *   flushes FILE           append and fsync forever: VICTIM flushed=K
 *
 * Unexpected errors print "FAULTS error <step>: <errno text>" and exit 70. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define BLOCK 4096
#define BLOCKS 8

static _Noreturn void fail(const char *step) {
    printf("FAULTS error %s: %s\n", step, strerror(errno));
    fflush(stdout);
    exit(70);
}
static unsigned char *aligned(void) {
    void *buffer;
    if (posix_memalign(&buffer, BLOCK, BLOCK)) fail("allocate");
    return buffer;
}
static void pattern(unsigned char *buffer, int block) {
    for (int i = 0; i < BLOCK; ++i) buffer[i] = (unsigned char)(block * 13 + i * 7);
}
static int prepare(const char *path) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    if (fd < 0) fail(path);
    unsigned char *buffer = aligned();
    for (int block = 0; block < BLOCKS; ++block) {
        pattern(buffer, block);
        if (pwrite(fd, buffer, BLOCK, (off_t)block * BLOCK) != BLOCK) fail("prepare write");
    }
    if (fsync(fd) || close(fd)) fail("prepare sync");
    puts("PREPARED");
    return 0;
}
static int read_blocks(const char *path, int count) {
    int fd = open(path, O_RDONLY | O_DIRECT | O_CLOEXEC);
    if (fd < 0) fail(path);
    unsigned char *buffer = aligned(), *expected = aligned();
    int ok = 0, eio = 0, corrupt = 0;
    for (int i = 0; i < count; ++i) {
        int block = i % BLOCKS;
        pattern(expected, block);
        ssize_t got = pread(fd, buffer, BLOCK, (off_t)block * BLOCK);
        if (got < 0 && errno == EIO) ++eio;
        else if (got != BLOCK) fail("read");
        else if (memcmp(buffer, expected, BLOCK)) ++corrupt;
        else ++ok;
    }
    close(fd);
    printf("READ ok=%d eio=%d corrupt=%d\n", ok, eio, corrupt);
    return 0;
}
static int write_blocks(const char *path, int count) {
    int fd = open(path, O_WRONLY | O_DIRECT | O_CLOEXEC);
    if (fd < 0) fail(path);
    unsigned char *buffer = aligned();
    int ok = 0, eio = 0;
    for (int i = 0; i < count; ++i) {
        int block = i % BLOCKS;
        pattern(buffer, block);
        ssize_t done = pwrite(fd, buffer, BLOCK, (off_t)block * BLOCK);
        if (done < 0 && errno == EIO) ++eio;
        else if (done != BLOCK) fail("write");
        else if (fdatasync(fd)) {
            if (errno != EIO) fail("write sync");
            ++eio;
        } else ++ok;
    }
    close(fd);
    printf("WRITE ok=%d eio=%d\n", ok, eio);
    return 0;
}
static unsigned long long monotonic_ns(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) fail("clock");
    return (unsigned long long)now.tv_sec * 1000000000ull + (unsigned long long)now.tv_nsec;
}
static int fsync_rounds(const char *path, int count) {
    int fd = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
    if (fd < 0) fail(path);
    unsigned long long low = ~0ull, high = 0;
    for (int i = 0; i < count; ++i) {
        if (write(fd, "fsync\n", 6) != 6) fail("append");
        unsigned long long before = monotonic_ns();
        if (fsync(fd)) fail("fsync");
        unsigned long long took = monotonic_ns() - before;
        if (took < low) low = took;
        if (took > high) high = took;
    }
    close(fd);
    printf("FSYNC min_ns=%llu max_ns=%llu\n", low, high);
    return 0;
}
static unsigned char *journal;
static long iterations, next_entry;
static void *record(void *identity) {
    for (long i = 0; i < iterations; ++i) {
        journal[__atomic_fetch_add(&next_entry, 1, __ATOMIC_SEQ_CST)] =
            (unsigned char)(uintptr_t)identity;
        (void)getppid();
    }
    return NULL;
}
static int race(long count) {
    iterations = count;
    journal = calloc((size_t)(2 * count), 1);
    if (!journal) fail("allocate");
    pthread_t a, b;
    if (pthread_create(&a, NULL, record, (void *)1) || pthread_create(&b, NULL, record, (void *)2))
        fail("pthread_create");
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    long switches = 0;
    for (long i = 1; i < 2 * count; ++i) switches += journal[i] != journal[i - 1];
    printf("RACE switches=%ld\n", switches);
    return 0;
}
static int flushes(const char *path) {
    int fd = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
    if (fd < 0) fail(path);
    for (unsigned long long count = 1;; ++count) {
        if (write(fd, "flush\n", 6) != 6 || fsync(fd)) fail("flush");
        printf("VICTIM flushed=%llu\n", count);
        fflush(stdout);
    }
}
int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "prepare")) return prepare(argv[2]);
    if (argc == 4 && !strcmp(argv[1], "read")) return read_blocks(argv[2], atoi(argv[3]));
    if (argc == 4 && !strcmp(argv[1], "write")) return write_blocks(argv[2], atoi(argv[3]));
    if (argc == 4 && !strcmp(argv[1], "fsync")) return fsync_rounds(argv[2], atoi(argv[3]));
    if (argc == 3 && !strcmp(argv[1], "race")) return race(atol(argv[2]));
    if (argc == 3 && !strcmp(argv[1], "flushes")) return flushes(argv[2]);
    return 64;
}
