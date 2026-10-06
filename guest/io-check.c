/* Live application probe, invoked before and after service restart. */
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <sys/io.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
    assert(argc == 2);
    printf("TARGET_IO_PROBE generation=%s\n", argv[1]); fflush(stdout);
    pid_t probe = fork();
    assert(probe >= 0);
    if (!probe) {
        /* One byte inside the supervisor's granted 0xf6..0xfa range. The
         * VMM ignores this unused start port, so a leaking negative control
         * cannot inject an acknowledgement or terminate a machine. */
        outb(0, 0xfa);
        _exit(0);
    }
    int status;
    assert(waitpid(probe, &status, 0) == probe);
    assert((WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV) &&
           "application inherited supervisor I/O permissions");
    printf("TARGET_IO_PORTS denied generation=%s\n", argv[1]);
    return 0;
}
