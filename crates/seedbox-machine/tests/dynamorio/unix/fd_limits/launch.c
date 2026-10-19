/* Limit only the owned application/runtime child, after the native supervisor
 * starts. Never adjust the test runner's or user's limits. */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <unistd.h>
int main(int argc, char **argv)
{
    if (argc < 3)
        return 2;
    struct rlimit limit;
    if (getrlimit(RLIMIT_NOFILE, &limit))
        return 2;
    limit.rlim_cur = strtoul(argv[1], NULL, 10);
    if (setrlimit(RLIMIT_NOFILE, &limit)) {
        perror("fd fixture host soft limit");
        return 2;
    }
    execv(argv[2], argv + 2);
    perror("fd fixture exec");
    return 2;
}
