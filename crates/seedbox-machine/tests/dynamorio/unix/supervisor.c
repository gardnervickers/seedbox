/* Own fork/exec descendants independently of the runtime under test. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>
static volatile sig_atomic_t cancelled;
static pid_t group;
static void cancel(int sig) { (void)sig; cancelled = 1; if (group > 0) kill(-group, SIGKILL); }
/* A failed application may leave descendants in a new process group/session.
 * As their subreaper, kill direct adoptees and reap until every owned child exits. */
static void kill_adoptees(void) {
    char path[96], text[65536];
    snprintf(path, sizeof path, "/proc/self/task/%d/children", getpid());
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) _exit(2);
    ssize_t n = read(fd, text, sizeof text - 1); close(fd);
    if (n < 0) _exit(2);
    text[n] = 0;
    char *at = text, *end;
    for (;;) {
        long pid = strtol(at, &end, 10);
        if (at == end) break;
        if (pid > 0) kill((pid_t)pid, SIGKILL);
        at = end;
    }
}
int main(int argc, char **argv) {
    if (argc < 2 || prctl(PR_SET_CHILD_SUBREAPER, 1)) return 2;
    struct sigaction a = {0}; a.sa_handler = cancel; sigemptyset(&a.sa_mask);
    if (sigaction(SIGTERM, &a, NULL)) return 2;
    group = fork();
    if (group < 0) return 2;
    if (!group) {
        signal(SIGTERM, SIG_DFL);
        if (setpgid(0, 0)) _exit(2);
        execv(argv[1], argv + 1); perror(argv[1]); _exit(2);
    }
    /* The child also sets this before exec, avoiding a parent/child race. */
    if (setpgid(group, group) && errno != EACCES && errno != ESRCH) return 2;
    int primary = 0, primary_seen = 0;
    for (;;) {
        if (cancelled || primary_seen) kill_adoptees();
        int status = 0;
        pid_t p = waitpid(-1, &status, (cancelled || primary_seen) ? WNOHANG : 0);
        if (p == group) { primary = status; primary_seen = 1; }
        else if (p < 0 && errno == ECHILD) break;
        else if (p < 0 && errno != EINTR) return 2;
        /* A successful wait is positive cleanup evidence. Re-polling after a
         * kill permits newly adopted descendants to be found on the next pass. */
    }
    if (cancelled) return 124;
    if (!primary_seen) return 2;
    return WIFEXITED(primary) ? WEXITSTATUS(primary) : 128 + WTERMSIG(primary);
}
