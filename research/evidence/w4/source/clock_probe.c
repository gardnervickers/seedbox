#define _GNU_SOURCE
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <sys/auxv.h>
#include <stdio.h>
#include <string.h>
#include <stdint.h>

int main(int argc, char **argv) {
    int direct = argc > 1 && strcmp(argv[1], "syscall") == 0;
    struct timespec value;
    uint64_t checksum = 0;
    for (int i=0; i<10000; i++) {
        int r = direct ? syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &value)
                       : clock_gettime(CLOCK_MONOTONIC, &value);
        if (r != 0) return 1;
        checksum ^= (uint64_t)value.tv_nsec;
    }
    printf("mode=%s calls=10000 vdso_present=%d checksum=%llu\n",
           direct ? "syscall" : "libc", getauxval(AT_SYSINFO_EHDR)!=0,
           (unsigned long long)checksum);
    return 0;
}
