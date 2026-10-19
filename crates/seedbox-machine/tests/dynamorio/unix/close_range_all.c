/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/close_range_all.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/* close_range_all MODE: daemon-style closing of every descriptor from 3 up, then ordinary work
 * (open, a thread, mmap of code, fork+exec of itself).
 *   range    close_range(3, ~0U, 0)
 *   cloexec  close_range(3, ~0U, CLOSE_RANGE_CLOEXEC)
 *   loop     close(fd) for fd 3..65535 (the classic loop; the translator refuses its own fds)
 * Natively prints "closed 0", "open 3", "thread ok", "code ok", "child 42" and "OK". */
#define _GNU_SOURCE
#include "support.h"
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
static void *thr(void *a) { return a; }
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child")) return 42;
    const char *mode = argc > 1 ? argv[1] : "range";
    setvbuf(stdout, NULL, _IONBF, 0);
    long r = 0;
    if (!strcmp(mode, "range")) r = syscall(SYS_close_range, 3, ~0U, 0);
    else if (!strcmp(mode, "cloexec")) r = syscall(SYS_close_range, 3, ~0U, 4 /* CLOSE_RANGE_CLOEXEC */);
    else for (int fd = 3; fd < 65536; fd++) close(fd);
    printf("closed %ld\n", r);
    int fd = open("/dev/null", O_RDONLY);
    printf("open %d\n", fd);
    pthread_t t;
    void *res = NULL;
    pthread_create(&t, NULL, thr, (void *)1);
    pthread_join(t, &res);
    printf("thread %s\n", res == (void *)1 ? "ok" : "bad");
    uint32_t *p = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unix_emit(p, 42);
    printf("code %s\n", ((int (*)(void))p)() == 42 ? "ok" : "bad");
    pid_t c = fork();
    if (c == 0) {
        char *av[] = {argv[0], "child", NULL};
        execve("/proc/self/exe", av, environ);
        _exit(127);
    }
    int st = -1;
    waitpid(c, &st, 0);
    printf("child %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    puts("OK");
    return r != 0 || fd != 3 || res != (void *)1 || !WIFEXITED(st) || WEXITSTATUS(st) != 42;
}
