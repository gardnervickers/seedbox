#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

/* Derived from R7's disarmed.c and itimer-fault.c. All values remain zero:
 * these controls never arm a timer and have no time or scheduling oracle. */
static int failures;
static int check_state(int which, const char *phase, long sec, long usec) {
    struct itimerval got;
    if (getitimer(which, &got)) return 2;
    long want_sec = which == ITIMER_REAL ? 0 : sec;
    long want_usec = which == ITIMER_REAL ? 0 : usec;
    int good = got.it_interval.tv_sec == want_sec &&
        got.it_interval.tv_usec == want_usec &&
        got.it_value.tv_sec == 0 && got.it_value.tv_usec == 0;
    printf("which=%d phase=%s interval=%ld.%06ld remaining=%ld.%06ld expected_interval=%ld.%06ld ok=%d\n",
           which, phase, (long)got.it_interval.tv_sec, (long)got.it_interval.tv_usec,
           (long)got.it_value.tv_sec, (long)got.it_value.tv_usec,
           want_sec, want_usec, good);
    failures += !good;
    return 0;
}
int main(void) {
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    void *readonly = mmap(NULL, page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (readonly == MAP_FAILED) return 2;
    for (int which = 0; which < 3; ++which) {
        struct itimerval a = {{17,1234},{0,0}}, b = {{29,5678},{0,0}};
        for (int c = 0; c < 10; ++c) {
            if (setitimer(which, &a, NULL)) return 2;
            struct itimerval old = {{-1,-1},{-1,-1}}, invalid = b;
            const void *in = &b;
            void *out = &old;
            int actual_which = which, expected_errno = 0;
            long expected_sec = 29, expected_usec = 5678;
            const char *name;
            switch (c) {
            case 0: name="valid-old"; break;
            case 1: name="bad-input"; in=(void *)1; expected_errno=EFAULT; expected_sec=17; expected_usec=1234; break;
            case 2: name="bad-old"; out=(void *)1; expected_errno=EFAULT; break;
            case 3: name="readonly-old"; out=readonly; expected_errno=EFAULT; break;
            case 4: name="invalid-which"; actual_which=3; expected_errno=EINVAL; expected_sec=17; expected_usec=1234; break;
            case 5: name="invalid-time"; invalid.it_interval.tv_usec=1000000; in=&invalid; expected_errno=EINVAL; expected_sec=17; expected_usec=1234; break;
            case 6: name="bad-input-and-which"; in=(void *)1; actual_which=-1; expected_errno=EFAULT; expected_sec=17; expected_usec=1234; break;
            case 7: name="invalid-time-bad-old"; invalid.it_value.tv_sec=-1; in=&invalid; out=(void *)1; expected_errno=EINVAL; expected_sec=17; expected_usec=1234; break;
            case 8: name="null-input"; in=NULL; expected_sec=0; expected_usec=0; break;
            default: name="aliased-input-old"; out=&b; break;
            }
            errno=0;
            long rc=syscall(SYS_setitimer, actual_which, in, out);
            int err=errno;
            int rc_good=expected_errno ? (rc==-1 && err==expected_errno) : rc==0;
            printf("which=%d phase=%s rc=%ld errno=%d expected_errno=%d ok=%d\n",which,name,rc,err,expected_errno,rc_good);
            failures += !rc_good;
            if (check_state(which,name,expected_sec,expected_usec)) return 2;
            if (rc==0) {
                struct itimerval *prev=out;
                long want_sec=which==ITIMER_REAL ? 0 : 17;
                long want_usec=which==ITIMER_REAL ? 0 : 1234;
                int good=prev->it_interval.tv_sec==want_sec && prev->it_interval.tv_usec==want_usec &&
                    prev->it_value.tv_sec==0 && prev->it_value.tv_usec==0;
                printf("which=%d phase=%s old_interval=%ld.%06ld old_remaining=%ld.%06ld ok=%d\n",which,name,
                       (long)prev->it_interval.tv_sec,(long)prev->it_interval.tv_usec,
                       (long)prev->it_value.tv_sec,(long)prev->it_value.tv_usec,good);
                failures += !good;
            }
        }
        struct itimerval zero={{0,0},{0,0}};
        if (setitimer(which,&zero,NULL)) return 2;
    }
    if (munmap(readonly,page)) return 2;
    printf("failures=%d\n",failures);
    return failures ? 1 : 0;
}
