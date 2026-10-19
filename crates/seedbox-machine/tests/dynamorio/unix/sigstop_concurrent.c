/* Seedbox source f32472b7fe470367b7eddecde1ac53ad299f3a3e, cfz.c t_sigstop:
 * all8 concurrent workers,2 JITs/2computes/4timed waiters,200 actual process
 * stops+continues and50 acknowledged process SIGUSR1. Original2/3ms subjects
 * and exact JIT64slots/16stride/value(i*7+3),20000term compute retained.
 * Positive kernel entry/work receipts replace incidental pacing sleeps.
 * Original elapsed early[] bit cannot be asserted under current time policy;
 * it remains PENDING and is never printed/asserted as zero or equivalent. */
#define _GNU_SOURCE
#include "support.h"
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <poll.h>

struct record {
    int stop,ready,usr1,tid[8];
    long jit_calls[2],jit_stale[2],sums[2],sum_bad[2],timed[4],eintr[4],wrong[4];
};
static struct record *shared;
static void fail(int line) {fprintf(stderr,"sigstop concurrent assertion line%d errno%d\n",line,errno);exit(1);}
static void handler(int sig) {if(sig!=SIGUSR1)_exit(3);__atomic_add_fetch(&shared->usr1,1,__ATOMIC_RELEASE);}
static uint64_t monotonic(void) {struct timespec t;if(clock_gettime(CLOCK_MONOTONIC,&t))_exit(2);return (uint64_t)t.tv_sec*1000000000+t.tv_nsec;}
static void *worker(void *arg) {
    long kind=(long)arg;sigset_t set;sigemptyset(&set);sigaddset(&set,SIGUSR1);
    if(pthread_sigmask(kind<4?SIG_UNBLOCK:SIG_BLOCK,&set,NULL))_exit(2);
    uint32_t *page=NULL,word=0;int pipefd[2]={-1,-1};uint64_t expected=0;
    if(kind<2) {
        page=mmap(NULL,4096,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
        if(page==MAP_FAILED)_exit(2);
    }
    if(kind==6&&pipe(pipefd))_exit(2);
    __atomic_store_n(&shared->tid[kind],gettid(),__ATOMIC_RELEASE);
    __atomic_add_fetch(&shared->ready,1,__ATOMIC_RELEASE);
    for(long i=0;!__atomic_load_n(&shared->stop,__ATOMIC_ACQUIRE);i++) {
        if(kind<2) {
            uint32_t *slot=page+(i%64)*4;uint16_t value=(uint16_t)(i*7+3);
            unix_emit(slot,value);unsigned actual=((unsigned (*)(void))slot)();
            if(actual!=value)__atomic_add_fetch(&shared->jit_stale[kind],1,__ATOMIC_RELEASE);
            __atomic_add_fetch(&shared->jit_calls[kind],1,__ATOMIC_RELEASE);continue;
        }
        if(kind<4) {
            uint64_t sum=0;for(volatile uint64_t x=0;x<20000;x++)sum+=(x*x)^(uint64_t)kind;
            if(!expected)expected=sum;
            if(sum!=expected)__atomic_add_fetch(&shared->sum_bad[kind-2],1,__ATOMIC_RELEASE);
            __atomic_add_fetch(&shared->sums[kind-2],1,__ATOMIC_RELEASE);continue;
        }
        struct timespec timeout={0,2000000};long result,want=0;
        switch(kind) {
        case 4:
            timeout.tv_nsec=3000000;want=-ETIMEDOUT;
            result=syscall(SYS_futex,&word,FUTEX_WAIT_PRIVATE,0,&timeout,NULL,0);break;
        case 5:
            /* seedbox-syscall-subject: nanosleep; bound-ms=2; invariant=actual blocked-entry before process SIGSTOP/CONT, source return0 and EINTR conservation across200cycles with concurrentJIT/compute. */
            result=syscall(SYS_nanosleep,&timeout,NULL);break;
        case 6: {
            struct pollfd pollfd={pipefd[0],POLLIN,0};
            result=syscall(SYS_ppoll,&pollfd,1,&timeout,NULL,8);break;
        }
        default: {
            uint64_t end=monotonic()+2000000;
            struct timespec deadline={(time_t)(end/1000000000),(long)(end%1000000000)};
            /* seedbox-syscall-subject: clock_nanosleep; bound-ms=2; invariant=actual blocked-entry before process SIGSTOP/CONT, original absolute deadline subject completes0 with no EINTR, no elapsed deadline assertion. */
            result=syscall(SYS_clock_nanosleep,CLOCK_MONOTONIC,TIMER_ABSTIME,&deadline,NULL);break;
        }
        }
        if(result<0)result=-errno;
        int index=(int)kind-4;
        if(result==-EINTR)__atomic_add_fetch(&shared->eintr[index],1,__ATOMIC_RELEASE);
        else if(result!=want)__atomic_add_fetch(&shared->wrong[index],1,__ATOMIC_RELEASE);
        __atomic_add_fetch(&shared->timed[index],1,__ATOMIC_RELEASE);
    }
    if(page&&munmap(page,4096))_exit(2);
    if(kind==6&&(close(pipefd[0])||close(pipefd[1])))_exit(2);
    return NULL;
}
static void child(void) {
    struct sigaction action={0};action.sa_handler=handler;action.sa_flags=SA_RESTART;
    sigemptyset(&action.sa_mask);if(sigaction(SIGUSR1,&action,NULL))_exit(2);
    sigset_t set;sigemptyset(&set);sigaddset(&set,SIGUSR1);
    if(pthread_sigmask(SIG_BLOCK,&set,NULL))_exit(2);
    pthread_t threads[8];
    for(long i=0;i<8;i++)if(pthread_create(&threads[i],NULL,worker,(void *)i))_exit(2);
    for(int i=0;i<8;i++)if(pthread_join(threads[i],NULL))_exit(2);
    _exit(0);
}
static void alive(pid_t pid) {
    int status;pid_t result=waitpid(pid,&status,WNOHANG);
    if(result==pid){fprintf(stderr,"sigstop child exited before work boundary: status%#x\n",status);exit(1);}
    if(result<0)fail(__LINE__);
}
static void blocked(pid_t pid,int index) {
    static const long nr[4]={SYS_futex,SYS_nanosleep,SYS_ppoll,SYS_clock_nanosleep};
    char path[96],text[512];int tid=__atomic_load_n(&shared->tid[index+4],__ATOMIC_ACQUIRE);
    snprintf(path,sizeof path,"/proc/%d/task/%d/syscall",pid,tid);
    for(;;) {
        alive(pid);int fd=open(path,O_RDONLY|O_CLOEXEC);if(fd<0)fail(__LINE__);
        ssize_t n=read(fd,text,sizeof text-1);close(fd);if(n<0)fail(__LINE__);text[n]=0;
        long got=-1;if(sscanf(text,"%ld",&got)==1&&got==nr[index])return;
        sched_yield();
    }
}
int main(void) {
    shared=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_SHARED|MAP_ANONYMOUS,-1,0);
    if(shared==MAP_FAILED)return 2;
    pid_t pid=fork();if(pid<0)return 2;if(!pid)child();
    while(__atomic_load_n(&shared->ready,__ATOMIC_ACQUIRE)!=8){alive(pid);sched_yield();}
    for(int i=0;i<4;i++)while(__atomic_load_n(&shared->timed[i],__ATOMIC_ACQUIRE)<1){alive(pid);sched_yield();}
    long prior[4]={0};int stopped=0,continued=0,sent=0;
    for(int cycle=0;cycle<200;cycle++) {
        for(int i=0;i<4;i++) {
            long *counter=i<2?&shared->jit_calls[i]:&shared->sums[i-2];
            while(__atomic_load_n(counter,__ATOMIC_ACQUIRE)<=prior[i]){alive(pid);sched_yield();}
            prior[i]=__atomic_load_n(counter,__ATOMIC_ACQUIRE);
        }
        blocked(pid,cycle%4);siginfo_t info={0};
        if(kill(pid,SIGSTOP)||waitid(P_PID,pid,&info,WSTOPPED|WEXITED)||info.si_code!=CLD_STOPPED||info.si_status!=SIGSTOP)fail(__LINE__);
        stopped++;
        if(kill(pid,SIGCONT)||waitid(P_PID,pid,&info,WCONTINUED|WEXITED)||info.si_code!=CLD_CONTINUED||info.si_status!=SIGCONT)fail(__LINE__);
        continued++;
        if(cycle%4==0) {
            int before=__atomic_load_n(&shared->usr1,__ATOMIC_ACQUIRE);
            if(kill(pid,SIGUSR1))fail(__LINE__);sent++;
            while(__atomic_load_n(&shared->usr1,__ATOMIC_ACQUIRE)==before){alive(pid);sched_yield();}
            if(__atomic_load_n(&shared->usr1,__ATOMIC_ACQUIRE)!=before+1)fail(__LINE__);
        }
    }
    __atomic_store_n(&shared->stop,1,__ATOMIC_RELEASE);
    int status;pid_t result;do result=waitpid(pid,&status,0);while(result<0&&errno==EINTR);
    if(result!=pid||!WIFEXITED(status)||WEXITSTATUS(status))fail(__LINE__);
    if(stopped!=200||continued!=200||sent!=50||shared->usr1!=50)fail(__LINE__);
    for(int i=0;i<2;i++) {
        if(shared->jit_calls[i]<1||shared->jit_stale[i]||shared->sums[i]<1||shared->sum_bad[i])fail(__LINE__);
        fprintf(stderr,"worker%d jit%ld stale%ld compute%ld bad%ld\n",i,shared->jit_calls[i],shared->jit_stale[i],shared->sums[i],shared->sum_bad[i]);
    }
    for(int i=0;i<4;i++) {
        if(shared->timed[i]<1||shared->eintr[i]||shared->wrong[i])fail(__LINE__);
        fprintf(stderr,"waiter%d calls%ld EINTR%ld wrong%ld early=PENDING\n",i,shared->timed[i],shared->eintr[i],shared->wrong[i]);
    }
    puts("OK sigstop concurrent: workers8 stopped200 continued200 usr1=50/50 JITstale0 computeBad0 timed4 EINTR0 wrong0 early=PENDING");
    if(munmap(shared,4096))return 2;return 0;
}
