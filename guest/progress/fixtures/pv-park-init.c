/* Dedicated PV fixture supervisor. The publisher, wait, progress helper and
 * nested application return all execute inside its one translated child. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef PARKED_MODE
#error PARKED_MODE must select wait=1, progress=2, or application-return=3
#endif

int main(void)
{
    if (mount("proc", "/proc", "proc", 0, NULL) ||
        mount("sysfs", "/sys", "sysfs", 0, NULL) ||
        mount("devtmpfs", "/dev", "devtmpfs", 0, NULL)) {
        perror("pv-park-init mount"); return 2;
    }
    pid_t child = fork();
    if (child < 0) return 2;
    if (child == 0) {
        char *args[] = {
            "/seedbox/progress/bin64/drrun",
#if PARKED_MODE == 1
            "-client", "/seedbox/progress/libseedbox_progress.so", "0",
            "--instructions-per-progress-call 50000 --progress-points-per-block 1",
            "-client", "/fixture/libparked_publication_client.so", "1", "/tmp/parked-target",
#endif
#ifdef PARKED_DEBUG
            "-log_to_stderr", "-loglevel", "3", "-logmask", "0x10",
#endif
            "-opt_speed", "-disable_traces", "-ldstex2cas", "-no_hw_cache_consistency",
            "-fake_ctr_dic", "-seedbox_icache_images",
#if PARKED_MODE == 2
            "-c", "/fixture/libparked_progress_client.so",
            "--instructions-per-progress-call", "50000", "--progress-points-per-block", "1",
            "/tmp/parked-target",
#elif PARKED_MODE == 3
            "-c", "/seedbox/progress/libseedbox_progress.so",
            "--instructions-per-progress-call", "50000", "--progress-points-per-block", "1",
#endif
            "--",
#if PARKED_MODE == 3
            "/fixture/parked-application-return",
#else
            "/fixture/parked-publication-context", "/tmp/parked-target",
#if PARKED_MODE == 2
            "progress",
#else
            "abandon",
#endif
#endif
            NULL,
        };
        execv(args[0], args);
        perror("pv-park-init exec"); _exit(127);
    }
    int status = -1;
    pid_t result;
    do { result = waitpid(child, &status, 0); } while (result < 0 && errno == EINTR);
    int ok = result == child && WIFEXITED(status) && WEXITSTATUS(status) == 0;
    printf("PV-PARK-END mode=%d success=%d status=%d\n", PARKED_MODE, ok, status);
    fflush(stdout);
    reboot(RB_POWER_OFF);
    return 2;
}
