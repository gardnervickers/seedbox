#define _POSIX_C_SOURCE 200809L
#include "guest_oom.h"
#include <assert.h>
#include <stdio.h>

/* Frame kernel log records; print each frame for the host decoder to check. */
static size_t frame_of(const char *record, char frame[OOM_REPORT_FRAME_BYTES]) {
    return oom_report_frame(record, strlen(record), 412, frame);
}

int main(void) {
    char frame[OOM_REPORT_FRAME_BYTES];
    const char *kills[] = {
        ("3,1843,122954950,-;Out of memory: Killed process 412 (nats-server) total-vm:1203328kB, "
         "anon-rss:446228kB, file-rss:1024kB, shmem-rss:0kB, UID:0 pgtables:1100kB oom_score_adj:0\n"
         " SUBSYSTEM=mem\n"),
        /* A memory cgroup kill; the task name holds spaces and parentheses. */
        ("3,9,1,c;Memory cgroup out of memory: Killed process 7 (a (b) c) total-vm:18446744073709551615kB, "
         "anon-rss:2kB, file-rss:3kB, shmem-rss:4kB, UID:1000 pgtables:5kB oom_score_adj:1000"),
        /* /dev/kmsg escapes unprintable task name bytes. */
        ("3,10,2,-;Out of memory: Killed process 4294967295 (\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a\\x0a) "
         "total-vm:1kB, anon-rss:1kB, file-rss:1kB, shmem-rss:1kB, UID:0 pgtables:1kB oom_score_adj:0\n"),
    };
    for (unsigned i = 0; i < sizeof(kills) / sizeof(*kills); ++i) {
        size_t size = frame_of(kills[i], frame);
        assert(size > 0 && size <= OOM_REPORT_FRAME_BYTES && frame[size - 1] == '\n');
        assert(fwrite(frame, 1, size, stdout) == size);
    }
    const char *ignored[] = {
        "",
        "3,1,1,-",
        /* Userspace may write the same text to /dev/kmsg, at a nonzero facility. */
        "11,1,1,-;Out of memory: Killed process 412 (x) total-vm:1kB, anon-rss:1kB, file-rss:1kB, shmem-rss:1kB",
        "4,2,1,-;oom_reaper: reaped process 412 (x), now anon-rss:0kB, file-rss:0kB, shmem-rss:0kB",
        "4,2,1,-;x invoked oom-killer: gfp_mask=0x140cca(GFP_HIGHUSER_MOVABLE|__GFP_COMP), order=0",
        "3,2,1,-;Out of memory: Killed process 4294967296 (x) total-vm:1kB, anon-rss:1kB, file-rss:1kB, shmem-rss:1kB",
        "3,2,1,-;Out of memory: Killed process 1 () total-vm:1kB, anon-rss:1kB, file-rss:1kB, shmem-rss:1kB",
        "3,2,1,-;Out of memory: Killed process 1 (x) total-vm:18446744073709551616kB, anon-rss:1kB, file-rss:1kB, shmem-rss:1kB",
        "3,2,1,-;Out of memory: Killed process 1 (x) total-vm:1kB, anon-rss:1kB, file-rss:1kB, shmem-rss:1",
        "3,2,1,-;Out of memory: Killed process 1 (x) total-vm:1kB, anon-rss:1kB\n, file-rss:1kB, shmem-rss:1kB",
        ("3,2,1,-;Out of memory: Killed process 1 (0123456789012345678901234567890123456789012345678901234567890) "
         "total-vm:1kB, anon-rss:1kB, file-rss:1kB, shmem-rss:1kB"),
    };
    for (unsigned i = 0; i < sizeof(ignored) / sizeof(*ignored); ++i) assert(!frame_of(ignored[i], frame));
}
