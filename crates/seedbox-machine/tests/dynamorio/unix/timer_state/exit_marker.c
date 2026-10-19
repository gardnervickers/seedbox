#define _GNU_SOURCE
#include <errno.h>
#include <sys/time.h>
#include <sys/syscall.h>
#include <unistd.h>
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
    timer_callbacks_ready();
    errno=0;
    return syscall(SYS_setitimer,77,NULL,NULL)!=-1 || errno!=EINVAL;
}
