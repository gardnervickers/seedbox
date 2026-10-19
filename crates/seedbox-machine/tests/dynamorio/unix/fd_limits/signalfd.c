/* Derived public controls for the signalfd allocation/update port defect.
 * Retain the public old-signalfd/signalfd4 axes and real signal
 * delivery oracle; no DR client or runtime internals are involved. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <unistd.h>

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "line %d: %s errno=%d\n", __LINE__, #condition, errno); \
        return 1; \
    } \
} while (0)

int
main(int argc, char **argv)
{
    CHECK(argc == 3);
    CHECK(!strcmp(argv[1], "old") || !strcmp(argv[1], "new"));
    CHECK(!strcmp(argv[2], "update"));
    const int old = !strcmp(argv[1], "old");
    long number = SYS_signalfd4;
#ifdef SYS_signalfd
    if (old)
        number = SYS_signalfd;
#else
    /* No old signalfd syscall in the A64 Linux UAPI. Never report a pass. */
    if (old)
        return 2;
#endif
    sigset_t mask, blocked;
    CHECK(sigemptyset(&mask) == 0);
    CHECK(sigaddset(&mask, SIGUSR1) == 0);
    blocked = mask;
    CHECK(sigaddset(&blocked, SIGUSR2) == 0);
    CHECK(sigprocmask(SIG_BLOCK, &blocked, NULL) == 0);
    int low = signalfd(-1, &mask, SFD_CLOEXEC | SFD_NONBLOCK);
    CHECK(low >= 0);
    const int fd = fcntl(low, F_DUPFD_CLOEXEC, 50);
    CHECK(fd >= 50);
    CHECK(close(low) == 0);
    struct rlimit limit;
    CHECK(getrlimit(RLIMIT_NOFILE, &limit) == 0);
    /* Limits belong only to this dedicated application process. */
    limit.rlim_cur = 32;
    CHECK(setrlimit(RLIMIT_NOFILE, &limit) == 0);
    CHECK(sigaddset(&mask, SIGUSR2) == 0);
    errno = 0;
    /* Linux x86-64/A64 kernel signal masks have 64 bits, unlike sigset_t.
     * Old signalfd takes three arguments; that ABI ignores the fourth. Flags
     * zero on update must preserve the existing CLOEXEC/NONBLOCK settings. */
    const long result = syscall(number, fd, &mask, 8, 0);
    CHECK(fcntl(fd, F_GETFD) == FD_CLOEXEC);
    CHECK(fcntl(fd, F_GETFL) & O_NONBLOCK);
    CHECK(result == fd);
    const int signals[] = { SIGUSR1, SIGUSR2 };
    const int count = 2;
    for (int i = 0; i < count; ++i) {
        /* Both signals were blocked before sending. A successful self-send
         * establishes pending state; read observes actual delivery, no sleep. */
        CHECK(kill(getpid(), signals[i]) == 0);
        struct signalfd_siginfo info;
        CHECK(read(fd, &info, sizeof(info)) == sizeof(info));
        CHECK(info.ssi_signo == (unsigned)signals[i]);
        CHECK(info.ssi_pid == (unsigned)getpid());
    }
    CHECK(fcntl(fd, F_GETFD) == FD_CLOEXEC);
    CHECK(fcntl(fd, F_GETFL) & O_NONBLOCK);
    CHECK(close(fd) == 0);
    printf("fd-limits signalfd: %s %s existing-fd flags and signal delivery preserved\n",
           argv[1], argv[2]);
    return 0;
}
