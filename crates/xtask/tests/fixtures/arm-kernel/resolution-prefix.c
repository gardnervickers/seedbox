
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static long resolution_ns;
static int getres(clockid_t id, struct timespec *res) {
    assert(id == CLOCK_MONOTONIC);
    if (resolution_ns < 0) { errno = EIO; return -1; }
    res->tv_sec = 0;
    res->tv_nsec = resolution_ns;
    return 0;
}
static void fail(const char *what) {
    fprintf(stderr, "%s: errno=%d\n", what, errno);
    exit(2);
}
#define clock_getres getres
