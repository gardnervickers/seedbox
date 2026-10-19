/* Source sub-invariants: Seedbox f32472b7 cfz.c t_restart RM_NANOSLEEP /
 * RM_CLOCKABS, stopContinue; flush_eintr:nanosleep (clock_nanosleep).
 * A bounded sleep is the syscall UNDER TEST, never readiness or pacing.
 * No elapsed-time/negative-absence oracle. Full remainder arithmetic and
 * success-at-deadline semantics remain separate unresolved source axes. */
#define _GNU_SOURCE
#include "support.h"
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <ucontext.h>
__attribute__((visibility("default"))) volatile int unix_waiter_tid;
UNIX_SUSPEND_MARKER;
long subject_call(long,long,long,long,long);
extern const char subject_after[];
#if defined(__aarch64__)
asm(".text\n.p2align 2\n.globl subject_call\n.type subject_call,%function\nsubject_call:\nmov x8,x4\nsvc #0\n.globl subject_after\nsubject_after:\nret\n");
#else
asm(".text\n.p2align 2\n.globl subject_call\n.type subject_call,@function\nsubject_call:\nmov %r8,%rax\nmov %rcx,%r10\nsyscall\n.globl subject_after\nsubject_after:\nret\n");
#endif
static atomic_int ready,returned,go;
static volatile sig_atomic_t handlers,nested,context_ok,mask_ok;
static int nesting,absolute,alias;
static struct timespec request,remainder,before;
static long result;
static void inner(int sig) { if(sig==SIGUSR2)nested++; }
static void handler(int sig,siginfo_t *si,void *uv) {
    ucontext_t *uc=uv;sigset_t current;pthread_sigmask(SIG_SETMASK,NULL,&current);
    mask_ok = sigismember(&current,SIGUSR1)==1 && sigismember(&current,SIGHUP)==1 && sigismember(&current,SIGWINCH)==1 &&
        sigismember(&uc->uc_sigmask,SIGWINCH)==1 && sigismember(&uc->uc_sigmask,SIGUSR1)==0;
#if defined(__aarch64__)
    context_ok = uc->uc_mcontext.pc==(uintptr_t)subject_after && (long)uc->uc_mcontext.regs[0]==-EINTR && !(uc->uc_mcontext.sp & 15);
#else
    context_ok = uc->uc_mcontext.gregs[REG_RIP]==(uintptr_t)subject_after && (long)uc->uc_mcontext.gregs[REG_RAX]==-EINTR;
#endif
    if(sig!=SIGUSR1 || si->si_code!=SI_TKILL || si->si_pid!=getpid())context_ok=0;
    if(nesting && syscall(SYS_tgkill,getpid(),gettid(),SIGUSR2))_exit(2);
    handlers++;
}
static void *waiter(void *unused) {
    (void)unused;unix_waiter_tid=gettid();
    sigset_t blocked;sigemptyset(&blocked);sigaddset(&blocked,SIGWINCH);pthread_sigmask(SIG_BLOCK,&blocked,NULL);
    atomic_store(&ready,1);
    if(absolute) {
        /* seedbox-syscall-subject: clock_nanosleep; bound-ms=60000; invariant=positive blocked-entry, hidden suspend/resume then final handled signal yields EINTR and unchanged deadline input. */
        result=subject_call(CLOCK_MONOTONIC,TIMER_ABSTIME,(long)&request,0,SYS_clock_nanosleep);
    } else {
        /* seedbox-syscall-subject: nanosleep; bound-ms=60000; invariant=positive blocked-entry, hidden suspend/resume then final handled signal yields EINTR and valid separate or aliased remainder. */
        result=subject_call((long)&request,(long)(alias?&request:&remainder),0,0,SYS_nanosleep);
    }
    atomic_store(&returned,1);return NULL;
}
static void observe(void) {
    char path[96],buf[256];snprintf(path,sizeof path,"/proc/self/task/%d/syscall",unix_waiter_tid);
    for(;;) {
        if(atomic_load(&returned)){fprintf(stderr,"sleep subject returned before final app signal: %ld\n",result);exit(1);}
        int fd=open(path,O_RDONLY|O_CLOEXEC);if(fd<0){perror(path);exit(2);}ssize_t n=read(fd,buf,sizeof buf-1);close(fd);if(n<0){perror(path);exit(2);}buf[n]=0;
        long nr=-1;if(sscanf(buf,"%ld",&nr)==1 && nr==(absolute?SYS_clock_nanosleep:SYS_nanosleep))return;
        sched_yield();
    }
}
int main(int argc,char **argv) {
    if(argc!=3)return 2;absolute=!strcmp(argv[1],"absolute");alias=!strcmp(argv[1],"alias");
    if(!absolute && !alias && strcmp(argv[1],"relative"))return 2;
    nesting=!strcmp(argv[2],"nested");int restart=nesting || !strcmp(argv[2],"restart");
    if(!restart && strcmp(argv[2],"plain"))return 2;
    request=(struct timespec){60,0};if(absolute){clock_gettime(CLOCK_MONOTONIC,&request);request.tv_sec+=60;}before=request;
    struct sigaction a={0};a.sa_sigaction=handler;a.sa_flags=SA_SIGINFO | (restart?SA_RESTART:0);sigemptyset(&a.sa_mask);sigaddset(&a.sa_mask,SIGHUP);
    if(sigaction(SIGUSR1,&a,NULL) || signal(SIGUSR2,inner)==SIG_ERR)return 2;
    pthread_t thread;if(pthread_create(&thread,NULL,waiter,NULL))return 2;
    while(!atomic_load(&ready))sched_yield();observe();
    unix_request_suspend();observe();
    if(syscall(SYS_tgkill,getpid(),unix_waiter_tid,SIGUSR1))return 2;
    pthread_join(thread,NULL);
    struct timespec rem=alias?request:remainder;
    int rem_ok=absolute ? request.tv_sec==before.tv_sec && request.tv_nsec==before.tv_nsec :
        rem.tv_sec>=0 && rem.tv_sec<=60 && rem.tv_nsec>=0 && rem.tv_nsec<1000000000 && (rem.tv_sec || rem.tv_nsec);
    if(result!=-EINTR || handlers!=1 || nested!=nesting || !context_ok || !mask_ok || !rem_ok)return 1;
    printf("OK sleep %s %s: EINTR, frame, mask, nested=%d, remainder/deadline-input valid\n",argv[1],argv[2],nested);return 0;
}
