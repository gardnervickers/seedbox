/* Guest application exercising the one attached disk, /dev/vda. Each boot
 * reports what earlier boots left durable, writes the next generation, prints
 * one marker per step on stdout, then waits for the host:
 *
 *   raw [SECONDS]  O_DIRECT, in 512-byte sectors:
 *                  sector 0 holds "SBXPROBE" and the generation count;
 *                  sector 8 (A) is written with the count, then FLUSHed;
 *                  sector 16 (B) is written afterwards and left unflushed;
 *                  sector 24 is reported but never written (base contents).
 *                  With SECONDS, B is FLUSHed after that much guest time.
 *   ext4           mounts /dev/vda at /mnt (commit=600, so only fsync or
 *                  sync commit): writes and fsyncs count and synced, then
 *                  writes dirty without any sync.
 *   volume DIR N   files on an already-mounted volume (a named volume backed
 *                  by the disk): reports the previous count, then for each of
 *                  N rounds writes a 64 KiB file, submits its data to the
 *                  device and waits for completion without any FLUSH
 *                  (sync_file_range), sleeps 1 s of guest time, then fsyncs it
 *                  and DIR. Finally fsyncs count, reads every round file back
 *                  with O_DIRECT, so from the device rather than the page
 *                  cache, and exits 0. It never waits.
 *   faults N [MS]  O_DIRECT on /dev/vda under disk faults: for each of N
 *                  rounds writes sector 64+R with a pattern, FLUSHes and
 *                  times the FLUSH in guest monotonic time, then reads the
 *                  sector back, reporting I/O errors and mismatches rather
 *                  than failing. With MS, sleeps that many milliseconds of
 *                  guest time after each round. Then waits.
 *   threads N      two threads each log their identity N times, with a
 *                  getppid syscall after each entry, and report how often
 *                  the log switches thread: one guest CPU switches rarely
 *                  unless the kernel yields at syscalls. Then waits.
 *
 * Report lines:
 *   DISK raw boot=N a=XX b=XX base=XX bytes=N
 *   DISK ext4 boot=N synced=S dirty=S          (S is "-" when absent)
 *   DISK volume boot=N, DISK volume wrote R, DISK volume submitted R,
 *   DISK volume synced R,
 *   DISK volume done count=N direct sum=HEX    (FNV-1a over all round files)
 *   DISK round=R write=W flush=F read=D flush_ns=T  (W, F: ok or eio; D: ok,
 *                                                     eio or corrupt)
 *   DISK faults done eio=E corrupt=C min_flush_ns=T max_flush_ns=T
 *   DISK threads switches=S
 * Failures print "DISK error <step>: <errno text>" and exit 70. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define SECTOR 512
#define RECORD 0
#define A 8
#define B 16
#define BASE 24

static _Noreturn void fail(const char *step) {
    printf("DISK error %s: %s\n", step, strerror(errno));
    fflush(stdout);
    exit(70);
}
static void marker(const char *format, unsigned long long value) {
    printf(format, value);
    fflush(stdout);
}
static _Noreturn void wait_forever(void) {
    for (;;) pause();
}
static unsigned char *sector_buffer(void) {
    void *buffer;
    if (posix_memalign(&buffer, 4096, SECTOR)) fail("allocate");
    return buffer;
}
static void sector_io(int fd, unsigned char *buffer, uint64_t sector, int write) {
    ssize_t done = write ? pwrite(fd, buffer, SECTOR, (off_t)(sector * SECTOR))
                         : pread(fd, buffer, SECTOR, (off_t)(sector * SECTOR));
    if (done != SECTOR) fail(write ? "write" : "read");
}
static int raw(int argc, char **argv) {
    int fd = open("/dev/vda", O_RDWR | O_DIRECT | O_CLOEXEC);
    if (fd < 0) fail("open /dev/vda");
    uint64_t bytes;
    if (ioctl(fd, BLKGETSIZE64, &bytes)) fail("size");
    unsigned char *buffer = sector_buffer();
    sector_io(fd, buffer, RECORD, 0);
    unsigned long long count = 0;
    if (!memcmp(buffer, "SBXPROBE", 8)) memcpy(&count, buffer + 8, sizeof(count));
    unsigned char seen[3];
    const uint64_t sectors[3] = {A, B, BASE};
    for (int i = 0; i < 3; ++i) {
        sector_io(fd, buffer, sectors[i], 0);
        seen[i] = buffer[0];
    }
    printf("DISK raw boot=%llu a=%02x b=%02x base=%02x bytes=%llu\n", count, seen[0], seen[1],
           seen[2], (unsigned long long)bytes);
    fflush(stdout);
    unsigned long long next = count + 1;
    memset(buffer, (int)(next & 0xff), SECTOR);
    sector_io(fd, buffer, A, 1);
    memset(buffer, 0, SECTOR);
    memcpy(buffer, "SBXPROBE", 8);
    memcpy(buffer + 8, &next, sizeof(next));
    sector_io(fd, buffer, RECORD, 1);
    /* fsync on a block device issues a virtio-blk FLUSH. */
    if (fsync(fd)) fail("flush durable");
    marker("DISK durable %llu\n", next);
    memset(buffer, (int)(next & 0xff), SECTOR);
    sector_io(fd, buffer, B, 1);
    marker("DISK volatile %llu\n", next);
    if (argc > 2) {
        struct timespec delay = {atoi(argv[2]), 0};
        while (nanosleep(&delay, &delay) && errno == EINTR) {}
        if (fsync(fd)) fail("flush volatile");
        marker("DISK flushed %llu\n", next);
    }
    wait_forever();
}
/* Whole small file contents, or "-" when absent. */
static void slurp(const char *path, char *out, size_t size) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        if (errno != ENOENT) fail(path);
        snprintf(out, size, "-");
        return;
    }
    ssize_t length = read(fd, out, size - 1);
    if (length < 0) fail(path);
    out[length] = 0;
    close(fd);
}
static void put(const char *path, unsigned long long value, int sync) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    if (fd < 0) fail(path);
    char text[32];
    int length = snprintf(text, sizeof(text), "%llu", value);
    if (write(fd, text, (size_t)length) != length) fail(path);
    if (sync && fsync(fd)) fail(path);
    if (close(fd)) fail(path);
}
static int ext4(void) {
    if (mkdir("/mnt", 0755) && errno != EEXIST) fail("mkdir /mnt");
    if (mount("/dev/vda", "/mnt", "ext4", 0, "commit=600")) fail("mount");
    char count_text[32], synced[32], dirty[32];
    slurp("/mnt/count", count_text, sizeof(count_text));
    slurp("/mnt/synced", synced, sizeof(synced));
    slurp("/mnt/dirty", dirty, sizeof(dirty));
    unsigned long long count = strcmp(count_text, "-") ? strtoull(count_text, NULL, 10) : 0;
    printf("DISK ext4 boot=%llu synced=%s dirty=%s\n", count, synced, dirty);
    fflush(stdout);
    unsigned long long next = count + 1;
    put("/mnt/count", next, 1);
    put("/mnt/synced", next, 1);
    marker("DISK synced %llu\n", next);
    put("/mnt/dirty", next, 0);
    marker("DISK dirty %llu\n", next);
    wait_forever();
}
#define ROUND_BYTES (64 * 1024)
static void fsync_path(const char *path, int flags) {
    int fd = open(path, flags | O_CLOEXEC);
    if (fd < 0 || fsync(fd) || close(fd)) fail(path);
}
/* Normal SDK applications see the named-volume path, not the supervisor's
 * raw /dev/vda. Leave a flushed generation and a dirty-only file for a public
 * crash/reboot; the next boot reports what actually survived on the device. */
static int volume_crash(const char *dir) {
    char path[512], count_text[32], synced[32], dirty[32];
    snprintf(path, sizeof(path), "%s/count", dir);
    slurp(path, count_text, sizeof(count_text));
    snprintf(path, sizeof(path), "%s/synced", dir);
    slurp(path, synced, sizeof(synced));
    snprintf(path, sizeof(path), "%s/dirty", dir);
    slurp(path, dirty, sizeof(dirty));
    unsigned long long count = strcmp(count_text, "-") ? strtoull(count_text, NULL, 10) : 0;
    printf("DISK volume-crash boot=%llu synced=%s dirty=%s\n", count, synced, dirty);
    fflush(stdout);
    unsigned long long next = count + 1;
    snprintf(path, sizeof(path), "%s/count", dir);
    put(path, next, 1);
    snprintf(path, sizeof(path), "%s/synced", dir);
    put(path, next, 1);
    fsync_path(dir, O_RDONLY | O_DIRECTORY);
    marker("DISK synced %llu\n", next);
    snprintf(path, sizeof(path), "%s/dirty", dir);
    put(path, next, 0);
    marker("DISK dirty %llu\n", next);
    wait_forever();
}
static int volume(const char *dir, int rounds) {
    char path[512], count_text[32];
    snprintf(path, sizeof(path), "%s/count", dir);
    slurp(path, count_text, sizeof(count_text));
    unsigned long long count = strcmp(count_text, "-") ? strtoull(count_text, NULL, 10) : 0;
    marker("DISK volume boot=%llu\n", count);
    unsigned char *data;
    if (posix_memalign((void **)&data, 4096, ROUND_BYTES)) fail("allocate");
    for (int round = 1; round <= rounds; ++round) {
        for (size_t i = 0; i < ROUND_BYTES; ++i)
            data[i] = (unsigned char)(i * 31u + (unsigned)round * 7u + count);
        snprintf(path, sizeof(path), "%s/round-%d", dir, round);
        int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
        if (fd < 0 || write(fd, data, ROUND_BYTES) != (ssize_t)ROUND_BYTES) fail(path);
        marker("DISK volume wrote %llu\n", (unsigned long long)round);
        /* Completed device writes, still unflushed: no FLUSH or FUA. */
        if (sync_file_range(fd, 0, 0, SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER)
            || close(fd))
            fail(path);
        marker("DISK volume submitted %llu\n", (unsigned long long)round);
        struct timespec delay = {1, 0};
        while (nanosleep(&delay, &delay) && errno == EINTR) {}
        fsync_path(path, O_RDONLY);
        fsync_path(dir, O_RDONLY | O_DIRECTORY);
        marker("DISK volume synced %llu\n", (unsigned long long)round);
    }
    snprintf(path, sizeof(path), "%s/count", dir);
    put(path, count + 1, 1);
    fsync_path(dir, O_RDONLY | O_DIRECTORY);
    uint64_t sum = 1469598103934665603ull;
    for (int round = 1; round <= rounds; ++round) {
        snprintf(path, sizeof(path), "%s/round-%d", dir, round);
        int fd = open(path, O_RDONLY | O_DIRECT | O_CLOEXEC);
        if (fd < 0) fail(path);
        ssize_t length;
        while ((length = read(fd, data, ROUND_BYTES)) > 0)
            for (ssize_t i = 0; i < length; ++i) sum = (sum ^ data[i]) * 1099511628211ull;
        if (length < 0 || close(fd)) fail(path);
    }
    printf("DISK volume done count=%llu direct sum=%016llx\n", count + 1, (unsigned long long)sum);
    fflush(stdout);
    return 0;
}
static unsigned long long monotonic_ns(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) fail("clock");
    return (unsigned long long)now.tv_sec * 1000000000ull + (unsigned long long)now.tv_nsec;
}
static const char *outcome(ssize_t done, ssize_t want) {
    if (done == want) return "ok";
    if (done < 0 && errno == EIO) return "eio";
    fail("unexpected I/O result");
}
static int faults(int rounds, long gap_ms) {
    int fd = open("/dev/vda", O_RDWR | O_DIRECT | O_CLOEXEC);
    if (fd < 0) fail("open /dev/vda");
    unsigned char *buffer = sector_buffer(), *back = sector_buffer();
    int eio = 0, corrupt = 0;
    unsigned long long min_flush = ~0ull, max_flush = 0;
    for (int round = 1; round <= rounds; ++round) {
        off_t at = (off_t)(64 + round) * SECTOR;
        for (int i = 0; i < SECTOR; ++i) buffer[i] = (unsigned char)(round * 7 + i);
        const char *write_result = outcome(pwrite(fd, buffer, SECTOR, at), SECTOR);
        unsigned long long before = monotonic_ns();
        const char *flush_result = fsync(fd) ? (errno == EIO ? "eio" : (fail("flush"), "")) : "ok";
        unsigned long long flush_ns = monotonic_ns() - before;
        if (flush_ns < min_flush) min_flush = flush_ns;
        if (flush_ns > max_flush) max_flush = flush_ns;
        memset(back, 0, SECTOR);
        ssize_t got = pread(fd, back, SECTOR, at);
        const char *read_result = outcome(got, SECTOR);
        if (got == SECTOR && !strcmp(write_result, "ok") && memcmp(back, buffer, SECTOR)) {
            read_result = "corrupt";
            ++corrupt;
        }
        eio += !strcmp(write_result, "eio") + !strcmp(flush_result, "eio") + !strcmp(read_result, "eio");
        printf("DISK round=%d write=%s flush=%s read=%s flush_ns=%llu\n", round, write_result,
               flush_result, read_result, flush_ns);
        fflush(stdout);
        struct timespec gap = {gap_ms / 1000, (gap_ms % 1000) * 1000000};
        while (gap_ms && nanosleep(&gap, &gap) && errno == EINTR) {}
    }
    printf("DISK faults done eio=%d corrupt=%d min_flush_ns=%llu max_flush_ns=%llu\n", eio, corrupt,
           min_flush, max_flush);
    fflush(stdout);
    wait_forever();
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
static int threads(long count) {
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
    printf("DISK threads switches=%ld\n", switches);
    fflush(stdout);
    wait_forever();
}
int main(int argc, char **argv) {
    if (argc >= 2 && !strcmp(argv[1], "raw")) return raw(argc, argv);
    if (argc == 2 && !strcmp(argv[1], "ext4")) return ext4();
    if (argc == 4 && !strcmp(argv[1], "volume")) return volume(argv[2], atoi(argv[3]));
    if (argc == 3 && !strcmp(argv[1], "volume-crash")) return volume_crash(argv[2]);
    if ((argc == 3 || argc == 4) && !strcmp(argv[1], "faults"))
        return faults(atoi(argv[2]), argc == 4 ? atol(argv[3]) : 0);
    if (argc == 3 && !strcmp(argv[1], "threads")) return threads(atol(argv[2]));
    return 64;
}
