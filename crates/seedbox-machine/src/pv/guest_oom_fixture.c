/* Native acceptance subject: kernel OOM kill versus an ordinary SIGKILL. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 2 || prctl(PR_SET_NAME, "oom-fixture")) return 64;
    printf("victim pid=%d\n", getpid());
    fflush(stdout);
    if (!strcmp(argv[1], "sigkill")) {
        kill(getpid(), SIGKILL);
        return 65;
    }
    if (strcmp(argv[1], "oom")) return 64;
    /* Each retained anonymous mapping is dirtied page by page. The supported
     * guest kernel must kill the allocating process; ENOMEM is a failed oracle. */
    for (;;) {
        volatile unsigned char *memory = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (memory == MAP_FAILED) return 70;
        for (unsigned i = 0; i < (1 << 20); i += 4096) memory[i] = 1;
    }
}
