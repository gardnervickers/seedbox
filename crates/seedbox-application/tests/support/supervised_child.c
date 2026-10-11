/* A guest application fixture. Rust checks its limits, signal state, output
 * and exit status through the supervisor's actual process boundary. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc < 2) return 127;
    if (!strcmp(argv[1], "exit-now")) return 1;
    if (!strcmp(argv[1], "exit-later")) {
        sigset_t mask;
        if (sigprocmask(SIG_SETMASK, NULL, &mask)) return 127;
        if (sigismember(&mask, SIGCHLD)) return 3;
        struct timespec delay = {.tv_nsec = 100000000}; nanosleep(&delay, NULL);
        return 7;
    }
    if (!strcmp(argv[1], "output-exit")) {
        const unsigned char out[] = {0, 255, '\n'}, err[] = {'e', 0, 128};
        int reopened = open("/proc/self/fd/1", O_WRONLY);
        if (reopened < 0 || write(reopened, out, sizeof(out)) != sizeof(out)) return 127;
        close(reopened);
        if (write(STDERR_FILENO, err, sizeof(err)) != sizeof(err)) return 127;
        return 9;
    }
    struct rlimit limit;
    if (getrlimit(RLIMIT_NOFILE, &limit)) return 127;
    if (dprintf(STDOUT_FILENO, "limit %llu %llu\n",
        (unsigned long long)limit.rlim_cur, (unsigned long long)limit.rlim_max) < 0) return 127;
    for (;;) pause();
}
