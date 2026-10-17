/* rlimit_nofile: lowers RLIMIT_NOFILE to 16, opens /dev/null until open fails, then execs
 * itself, which prints the limit it inherited.
 * Natively prints "limit 16 16", "opened 13 until EMFILE", "exec'd limit 16 16" and "OK". */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>
extern char **environ;
int main(int argc, char **argv) {
    struct rlimit rl;
    if (argc > 1) {
        getrlimit(RLIMIT_NOFILE, &rl);
        printf("exec'd limit %lu %lu\n%s\n", (unsigned long)rl.rlim_cur, (unsigned long)rl.rlim_max, rl.rlim_cur == 16 ? "OK" : "FAIL");
        return rl.rlim_cur != 16;
    }
    rl.rlim_cur = rl.rlim_max = 16;
    if (setrlimit(RLIMIT_NOFILE, &rl)) { perror("setrlimit"); return 2; }
    getrlimit(RLIMIT_NOFILE, &rl);
    printf("limit %lu %lu\n", (unsigned long)rl.rlim_cur, (unsigned long)rl.rlim_max);
    int n = 0, e = 0;
    for (; n < 64; n++)
        if (open("/dev/null", O_RDONLY | O_CLOEXEC) < 0) { e = errno; break; }
    printf("opened %d until %s\n", n, e ? strerrorname_np(e) : "none");
    fflush(stdout);
    char *av[] = {argv[0], "child", NULL};
    execve("/proc/self/exe", av, environ);
    perror("execve");
    return 2;
}
