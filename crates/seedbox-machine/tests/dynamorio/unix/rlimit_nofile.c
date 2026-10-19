/* Adapted from PR317's rlimit_nofile control. Exact native open counts and
 * exhausted exec success are superseded by the cooperative resource contract.
 * This fixture checks actual kernel limits and ordinary fork/exec instead. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#define CHECK(c) do { if (!(c)) { \
    fprintf(stderr, "rlimit_nofile line%d: %s errno=%d\n", __LINE__, #c, errno); \
    return 2; } } while (0)

static int
limits(rlim_t soft, rlim_t hard)
{
    struct rlimit libc_limit, kernel_limit;
    CHECK(getrlimit(RLIMIT_NOFILE, &libc_limit) == 0);
    CHECK(syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &kernel_limit) == 0);
    CHECK(libc_limit.rlim_cur == soft && libc_limit.rlim_max == hard);
    CHECK(kernel_limit.rlim_cur == soft && kernel_limit.rlim_max == hard);
    return 0;
}

int
main(int argc, char **argv)
{
    struct rlimit current;
    CHECK(getrlimit(RLIMIT_NOFILE, &current) == 0);
    if (argc == 3 && !strcmp(argv[1], "child")) {
        CHECK(limits(2048, (rlim_t)strtoull(argv[2], NULL, 10)) == 0);
        puts("exec'd limit 2048");
        return 0;
    }
    CHECK(current.rlim_cur >= 8192 && current.rlim_max >= 8192);
    CHECK(limits(current.rlim_cur, current.rlim_max) == 0);
    if (argc == 2 && !strcmp(argv[1], "boundary")) {
        CHECK(current.rlim_cur == 8192);
        printf("READY %ld\n", (long)getpid());
        CHECK(fflush(stdout) == 0);
        char acknowledgement;
        CHECK(read(STDIN_FILENO, &acknowledgement, 1) == 1);
        CHECK(acknowledgement == 'x');
        CHECK(limits(6144, current.rlim_max) == 0);
    } else {
        CHECK(argc == 1);
    }
    struct rlimit next = { .rlim_cur = 4096, .rlim_max = current.rlim_max };
    CHECK(syscall(SYS_setrlimit, RLIMIT_NOFILE, &next) == 0);
    CHECK(limits(4096, current.rlim_max) == 0);
    next.rlim_cur = 2048;
    struct rlimit old;
    CHECK(prlimit(0, RLIMIT_NOFILE, &next, &old) == 0);
    CHECK(old.rlim_cur == 4096 && old.rlim_max == current.rlim_max);
    CHECK(limits(2048, current.rlim_max) == 0);
    char hard[32];
    snprintf(hard, sizeof(hard), "%llu", (unsigned long long)current.rlim_max);
    CHECK(fflush(stdout) == 0);
    pid_t child = fork();
    CHECK(child >= 0);
    if (child == 0) {
        if (limits(2048, current.rlim_max))
            _exit(2);
        execl("/proc/self/exe", argv[0], "child", hard, NULL);
        _exit(2);
    }
    int status;
    CHECK(waitpid(child, &status, 0) == child);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    execl("/seedbox-fd-test-no-such-executable", "missing", NULL);
    CHECK(errno == ENOENT);
    CHECK(limits(2048, current.rlim_max) == 0);
    puts("limits kernel-visible set/get/prlimit fork/exec preserved");
    puts("OK");
    return 0;
}
