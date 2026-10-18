/* Privileged guest application that ends the whole kernel, not just itself.
 * argv[1] selects the terminal path:
 *   reboot    reboot(2) RB_AUTOBOOT: architectural reset request
 *   halt      reboot(2) RB_HALT_SYSTEM: permanent halt
 *   poweroff  reboot(2) RB_POWER_OFF
 *   panic     sysrq 'c': a real kernel panic (the kernel then restarts with panic=-1)
 *   idle      print the marker and sleep forever (a live peer for Driver tests)
 * The marker is flushed and given time to leave the guest before the kernel
 * stops; a failed request exits with status 70, which is a process exit only. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/reboot.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) return 64;
    const char *mode = argv[1];
    if (!strcmp(mode, "idle")) {
        printf("KERNEL_TERMINAL idle\n");
        fflush(stdout);
        for (;;) pause();
    }
    int command;
    if (!strcmp(mode, "reboot")) command = RB_AUTOBOOT;
    else if (!strcmp(mode, "halt")) command = RB_HALT_SYSTEM;
    else if (!strcmp(mode, "poweroff")) command = RB_POWER_OFF;
    else if (!strcmp(mode, "panic")) command = 0;
    else return 64;
    printf("KERNEL_TERMINAL %s\n", mode);
    fflush(stdout);
    /* Let the supervisor forward the marker before the kernel stops. */
    struct timespec delay = {0, 20 * 1000 * 1000};
    nanosleep(&delay, NULL);
    if (command) {
        reboot(command);
    } else {
        int fd = open("/proc/sysrq-trigger", O_WRONLY);
        if (fd >= 0) (void)!write(fd, "c", 1);
    }
    dprintf(2, "kernel-terminal: %s request returned\n", mode);
    return 70;
}
