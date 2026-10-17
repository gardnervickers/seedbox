/* flush_eintr CALL ROUNDS [PERIOD_US]: a thread blocks in CALL until main wakes it (2 ms
 * later, ROUNDS times) while another thread rewrites and flushes code every PERIOD_US
 * (default 200; 0: no such thread). No signal is sent to the waiting thread, so natively
 * the call never fails with EINTR. CALL: epoll (epoll_pwait), ppoll, pselect, nanosleep
 * (clock_nanosleep, woken by its own 2 ms timeout), read (pipe read; restartable control).
 * Prints "eintr N" (natively 0) and "OK". */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <time.h>
#include <unistd.h>
static int rounds, period = 200, stop, fds[2], ep, eintr, turn;
static const char *call;
static void *flusher(void *a) {
    (void)a;
    uint32_t *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (uint32_t i = 0; !__atomic_load_n(&stop, __ATOMIC_ACQUIRE); i++) {
        code[0] = 0x52800000u | (i & 0xffff) << 5;
        code[1] = 0xd65f03c0u;
        __builtin___clear_cache((char *)code, (char *)(code + 2));
        ((int (*)(void))code)();
        usleep(period);
    }
    return NULL;
}
static void *waiter(void *a) {
    (void)a;
    for (int r = 1; r <= rounds; r++) {
        __atomic_store_n(&turn, r, __ATOMIC_RELEASE);
        int rc;
        for (;;) {
            if (!strcmp(call, "epoll")) {
                struct epoll_event ev;
                rc = epoll_wait(ep, &ev, 1, -1);
            } else if (!strcmp(call, "ppoll")) {
                struct pollfd pf = {fds[0], POLLIN, 0};
                rc = ppoll(&pf, 1, NULL, NULL);
            } else if (!strcmp(call, "pselect")) {
                fd_set s;
                FD_ZERO(&s);
                FD_SET(fds[0], &s);
                rc = pselect(fds[0] + 1, &s, NULL, NULL, NULL, NULL);
            } else if (!strcmp(call, "nanosleep")) {
                struct timespec t = {0, 2000000};
                rc = clock_nanosleep(CLOCK_MONOTONIC, 0, &t, NULL) ? -1 : 0;
                if (rc) errno = EINTR;
                if (!rc) break;
            } else {
                char b;
                rc = (int)read(fds[0], &b, 1);
                if (rc == 1) break;
            }
            if (rc >= 0) break;
            if (errno != EINTR) { perror(call); exit(2); }
            eintr++;
        }
        if (strcmp(call, "read") && strcmp(call, "nanosleep")) {
            char b;
            if (read(fds[0], &b, 1) != 1) exit(2);
        }
    }
    return NULL;
}
int main(int argc, char **argv) {
    call = argv[1];
    rounds = atoi(argv[2]);
    if (argc > 3) period = atoi(argv[3]);
    if (pipe(fds)) return 2;
    ep = epoll_create1(0);
    struct epoll_event ev = {EPOLLIN, {0}};
    epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &ev);
    pthread_t f, w;
    if (period) pthread_create(&f, NULL, flusher, NULL);
    pthread_create(&w, NULL, waiter, NULL);
    for (int r = 1; r <= rounds; r++) {
        while (__atomic_load_n(&turn, __ATOMIC_ACQUIRE) < r) usleep(100);
        usleep(2000);
        if (strcmp(call, "nanosleep") && write(fds[1], "x", 1) != 1) return 2;
    }
    pthread_join(w, NULL);
    __atomic_store_n(&stop, 1, __ATOMIC_RELEASE);
    if (period) pthread_join(f, NULL);
    printf("eintr %d\n", eintr);
    puts("OK");
    return 0;
}
