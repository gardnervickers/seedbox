#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/random.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/io.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/random.h>
#include <time.h>
#include <unistd.h>

static void die(const char *why) {
    perror(why);
    fflush(stdout);
    if (!ioperm(0xf4, 1, 1)) outb(2, 0xf4);
    for (;;) pause();
}

int main(int argc, char **argv) {
    if (argc == 2) {
        char *end;
        unsigned long long ns = strtoull(argv[1], &end, 10);
        if (!*argv[1] || *end) return 2;
        struct timespec deadline = {ns / 1000000000, ns % 1000000000};
        int error;
        while ((error = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &deadline, NULL)) == EINTR) {}
        return error ? 2 : 0;
    }
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL)) die("devtmpfs");
    int fd = open("/dev/ttyS0", O_RDWR | O_NOCTTY);
    if (fd < 0) die("serial");
    for (int i = 0; i < 3; ++i) if (dup2(fd, i) < 0) die("dup2");
    if (fd > 2) close(fd);
    setvbuf(stdout, NULL, _IONBF, 0);
    if (mount("proc", "/proc", "proc", 0, NULL)) die("proc");
    struct { int entropy_count, buf_size; unsigned char bytes[32]; } seed = {256, 32, {0}};
    fd = open("/seed", O_RDONLY);
    if (fd < 0 || read(fd, seed.bytes, 32) != 32) die("seed");
    close(fd);
    fd = open("/dev/random", O_RDWR);
    if (fd < 0 || ioctl(fd, RNDADDENTROPY, &seed)) die("RNDADDENTROPY");
    close(fd);
    unsigned char sample[32];
    if (getrandom(sample, sizeof sample, GRND_NONBLOCK) != sizeof sample) die("getrandom");
    printf("TCG_ENTROPY ");
    for (unsigned i = 0; i < sizeof sample; ++i) printf("%02x", sample[i]);
    printf("\n");
    char *args[] = {"/bin/busybox", "sh", "/fixture.init", NULL};
    char *env[] = {"PATH=/bin:/usr/bin:/sbin:/usr/sbin", NULL};
    execve(args[0], args, env);
    die("exec fixture");
}
