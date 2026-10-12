/* Dedicated real-kernel PV horizon fixture. Setup is the init supervisor;
 * after exec, the syscall-less application loop is entirely translated. */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    if (argc == 2 && !strcmp(argv[1], "--spin")) {
        puts("PV-PROGRESS-READY");
        fflush(stdout);
        for (;;)
            __asm__ volatile("nop" ::: "memory");
    }
    if (mount("proc", "/proc", "proc", 0, NULL) ||
        mount("sysfs", "/sys", "sysfs", 0, NULL) ||
        mount("devtmpfs", "/dev", "devtmpfs", 0, NULL)) {
        perror("pv-spin-init mount");
        return 2;
    }
    char *args[] = {
        "/seedbox/progress/bin64/drrun", "-opt_speed", "-disable_traces",
        "-ldstex2cas", "-no_hw_cache_consistency", "-fake_ctr_dic",
        "-seedbox_icache_images", "-c", "/seedbox/progress/libseedbox_progress.so",
        "--instructions-per-progress-call", "50000", "--progress-points-per-block", "1",
        "--", "/init", "--spin", NULL,
    };
    execv(args[0], args);
    perror("pv-spin-init exec");
    return 2;
}
