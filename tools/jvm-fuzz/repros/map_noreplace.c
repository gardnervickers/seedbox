#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
int main(void) {
    char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    p[0] = 42;
    errno = 0;
    char *q = mmap(p, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    int e = errno;
    printf("p=%p q=%p failed=%d errno=%s\n", (void *)p, (void *)q, q == MAP_FAILED, strerror(e));
    fflush(stdout);
    printf("p[0]=%d\n", p[0]);
    return 0;
}
