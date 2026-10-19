/* Source port: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e
 * tools/jvm-fuzz/repros/flush_eintr.c. Each request is qualified by actual
 * kernel blocking, followed by a positive JIT rewrite/call acknowledgement.
 * The bounded clock_nanosleep call is itself the syscall subject. */
#define _GNU_SOURCE
#include "support.h"
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <time.h>
__attribute__((visibility("default"))) volatile int unix_waiter_tid;
UNIX_SUSPEND_MARKER;
static atomic_int stop, turn, errors, calls, flushes, qualified;
static int fds[2], ep, rounds;
static const char *mode;
static long number;
static void *flusher(void *unused) {
    (void)unused;
    uint32_t *code=mmap(NULL,4096,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(code==MAP_FAILED)abort();
    for(unsigned i=0;!atomic_load(&stop);i++) {
        unix_emit(code,i & 0xffff);
        if(((unsigned (*)(void))code)()!=(i & 0xffff))abort();
        atomic_fetch_add(&flushes,1);
        sched_yield();
    }
    munmap(code,4096);return NULL;
}
static void *waiter(void *unused) {
    (void)unused;unix_waiter_tid=gettid();
    for(int r=1;r<=rounds;r++) {
        atomic_store(&turn,r);
        int result;
        do {
            if(!strcmp(mode,"epoll")) { struct epoll_event e;result=epoll_wait(ep,&e,1,-1); }
            else if(!strcmp(mode,"ppoll")) { struct pollfd p={fds[0],POLLIN,0};result=ppoll(&p,1,NULL,NULL); }
            else if(!strcmp(mode,"pselect")) { fd_set s;FD_ZERO(&s);FD_SET(fds[0],&s);result=pselect(fds[0]+1,&s,NULL,NULL,NULL,NULL); }
            else if(!strcmp(mode,"nanosleep")) {
                /* Original selector is libc relative clock_nanosleep, NULL
                 * remainder. Preserve its positive error-number convention. */
                struct timespec request={0,200000000};
                /* seedbox-syscall-subject: clock_nanosleep; bound-ms=200; invariant=no EINTR from qualified hidden suspend/flush before successful completion. */
                int err=clock_nanosleep(CLOCK_MONOTONIC,0,&request,NULL);
                result=err?-1:0;if(err)errno=err;
            } else { char b;result=read(fds[0],&b,1); }
            if(result<0) { if(errno!=EINTR)abort();atomic_fetch_add(&errors,1); }
            atomic_fetch_add(&calls,1);
        } while(result<0);
        if(result!=(!strcmp(mode,"nanosleep")?0:1))abort();
        if(!strcmp(mode,"nanosleep"))while(atomic_load(&qualified)<r)sched_yield();
        if(strcmp(mode,"read") && strcmp(mode,"nanosleep")) { char b;if(read(fds[0],&b,1)!=1)abort(); }
    }
    return NULL;
}
int main(int argc,char **argv) {
    if(argc!=3)return 2;mode=argv[1];rounds=atoi(argv[2]);
    if(rounds<1)return 2;
    if(!strcmp(mode,"epoll")) {
#if defined(__aarch64__)
        number=SYS_epoll_pwait;
#else
        number=SYS_epoll_wait;
#endif
    } else if(!strcmp(mode,"ppoll"))number=SYS_ppoll;
    else if(!strcmp(mode,"pselect"))number=SYS_pselect6;
    else if(!strcmp(mode,"read"))number=SYS_read;
    else if(!strcmp(mode,"nanosleep"))number=SYS_clock_nanosleep;
    else return 2;
    if(pipe(fds))return 2;
    ep=epoll_create1(0);struct epoll_event e={.events=EPOLLIN};
    if(ep<0 || epoll_ctl(ep,EPOLL_CTL_ADD,fds[0],&e))return 2;
    pthread_t fl,wt;if(pthread_create(&fl,NULL,flusher,NULL)||pthread_create(&wt,NULL,waiter,NULL))return 2;
    for(int r=1;r<=rounds;r++) {
        while(atomic_load(&turn)<r)sched_yield();
        unix_wait_syscall(getpid(),unix_waiter_tid,number);
        unix_request_suspend();
        int before=atomic_load(&flushes);
        while(atomic_load(&flushes)<=before)sched_yield();
        if(strcmp(mode,"nanosleep") && write(fds[1],"x",1)!=1)abort();
        atomic_store(&qualified,r);
    }
    pthread_join(wt,NULL);atomic_store(&stop,1);pthread_join(fl,NULL);
    printf("eintr %d\ncompleted %d/%d\n",atomic_load(&errors),rounds,rounds);
    puts(atomic_load(&errors)?"FAIL":"OK");
    return atomic_load(&errors)!=0 || atomic_load(&calls)!=rounds;
}
