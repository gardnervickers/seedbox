#define _GNU_SOURCE
#include <errno.h>
#include <sys/time.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <signal.h>
#include <stdio.h>
static volatile sig_atomic_t hits[3];
static void handler(int sig) { hits[sig==SIGALRM ? 0 : sig==SIGVTALRM ? 1 : 2]++; }
__attribute__((noinline,visibility("default"))) void timer_callbacks_ready(void) {
#if defined(__aarch64__)
    /* Actual application instruction at the exported marker on A64 too. */
    __asm__ volatile("tst x0,x0" ::: "cc", "memory");
#elif defined(__x86_64__)
    __asm__ volatile("test %%rax,%%rax" ::: "cc", "memory");
#else
#error "timer callback marker requires x86-64 or A64"
#endif
 }
int main(void) {
    struct itimerval ignored;
    for(int i=0;i<100;i++) if(getitimer(ITIMER_REAL,&ignored)) return 2;
    struct sigaction action = { .sa_handler = handler };
    sigemptyset(&action.sa_mask);
    const int signals[3] = { SIGALRM, SIGVTALRM, SIGPROF };
    sigset_t blocked, observed;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &blocked, NULL)) return 3;
    for (int i=0; i<3; i++) {
        struct itimerval one = { .it_value = { 0, 1000 } };
        if (sigaction(signals[i], &action, NULL) || setitimer(i, &one, NULL)) return 4;
    }
    while (!hits[0] || !hits[1] || !hits[2]) {
        for (volatile int i=0; i<10000; i++) {}
    }
    if (sigprocmask(SIG_SETMASK, NULL, &observed) || !sigismember(&observed, SIGUSR1)) return 5;
    for (int i=0; i<3; i++) {
        struct itimerval zero = {0};
        if (setitimer(i, &zero, NULL)) return 6;
        if (sigismember(&observed, signals[i])) return 7;
    }
    printf("app-timer receipts=%d,%d,%d mask=preserved\n", hits[0], hits[1], hits[2]);
    timer_callbacks_ready();
    errno=0;
    return syscall(SYS_setitimer,77,NULL,NULL)!=-1 || errno!=EINVAL;
}
