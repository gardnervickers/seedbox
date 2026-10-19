/* Source-derived no-sleep sub-invariants from Seedbox PR317 f32472b7:
 * cfz.c t_restart.stopContinue, t_sigstop; flush_eintr.c; DR 60c44f865.
 * Snapshot is a distinct proposed shared-helper discriminator, NOT sleep
 * remainder/deadline coverage. Causal old/fixed evidence is recorded separately. */
#define _GNU_SOURCE
#include "support.h"
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <time.h>
__attribute__((visibility("default"))) volatile int unix_waiter_tid;
UNIX_SUSPEND_MARKER;
struct state { int word; struct timespec original; };
struct result { long value; int error, payload, stop_eintr; };
static void fail(int line) { fprintf(stderr,"fixture waits assertion at line %d errno %d\n",line,errno);exit(1); }
static struct state *shared;
static int control[2], confirmed[2], result_pipe[2], data[2], ep;
static const char *mode;
static int snapshot, stopped;
static void byte(int fd, char c) { if(write(fd,&c,1)!=1)fail(__LINE__); }
static void take(int fd) { char c;if(read(fd,&c,1)!=1)fail(__LINE__); }
static void *controller(void *unused) {
    (void)unused;take(control[0]);
    unix_request_suspend();
    uint32_t *code=mmap(NULL,4096,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(code==MAP_FAILED)fail(__LINE__);
    for(unsigned i=0;i<100;i++) { unix_emit(code,i);if(((unsigned (*)(void))code)()!=i)fail(__LINE__); }
    munmap(code,4096);
    byte(confirmed[1],'C');return NULL;
}
static long nr(void) {
    if(!strcmp(mode,"futex") || snapshot)return SYS_futex;
    if(!strcmp(mode,"ppoll"))return SYS_ppoll;
    if(!strcmp(mode,"pselect"))return SYS_pselect6;
    if(!strcmp(mode,"epoll"))return SYS_epoll_pwait;
    return SYS_read;
}
static struct result wait_once(void) {
    struct result r={0};
    if(!strcmp(mode,"futex") || snapshot) {
        r.value=syscall(SYS_futex,&shared->word,FUTEX_WAIT,0,&shared->original,NULL,0);
        r.payload=__atomic_load_n(&shared->word,__ATOMIC_ACQUIRE);
    } else if(!strcmp(mode,"ppoll")) {
        struct pollfd p={data[0],POLLIN,0};r.value=ppoll(&p,1,&shared->original,NULL);r.payload=p.revents==POLLIN;
    } else if(!strcmp(mode,"pselect")) {
        fd_set s;FD_ZERO(&s);FD_SET(data[0],&s);
        r.value=pselect(data[0]+1,&s,NULL,NULL,&shared->original,NULL);r.payload=FD_ISSET(data[0],&s);
    } else if(!strcmp(mode,"epoll")) {
        struct epoll_event e={0};r.value=syscall(SYS_epoll_pwait,ep,&e,1,600000,NULL,8);r.payload=e.data.u64==0x5eed && e.events==EPOLLIN;
    } else { char c=0;r.value=read(data[0],&c,1);r.payload=c=='W'; }
    r.error=r.value<0?errno:0;return r;
}
static void observed(pid_t child) {
    /* A single wait per image: no normal reentry can masquerade as this phase.
     * A result record is also a positive event, and exposes early EINTR. */
    char path[96],buf[512];snprintf(path,sizeof path,"/proc/%d/task/%d/syscall",child,child);
    for(;;) {
        struct pollfd ready={result_pipe[0],POLLIN,0};
        if(poll(&ready,1,0)>0) { struct result r;if(read(result_pipe[0],&r,sizeof r)!=sizeof r)fail(__LINE__);fprintf(stderr,"wait returned before qualified completion: %ld errno %d\n",r.value,r.error);exit(1); }
        int fd=open(path,O_RDONLY|O_CLOEXEC);if(fd<0){perror(path);exit(2);}
        ssize_t n=read(fd,buf,sizeof buf-1);close(fd);if(n<0){perror(path);exit(2);}buf[n]=0;
        long number=-1;unsigned long arg=0;
        if(sscanf(buf,"%ld %lx",&number,&arg)>=1 && stopped && !strcmp(mode,"futex") && number==SYS_restart_syscall)return;
        if(sscanf(buf,"%ld %lx",&number,&arg)==2 && number==nr() &&
           ((!strcmp(mode,"futex") || snapshot) ? arg==(uintptr_t)&shared->word :
            !strcmp(mode,"epoll") ? arg==(unsigned long)ep :
            !strcmp(mode,"read") ? arg==(unsigned long)data[0] : 1))return;
        sched_yield();
    }
}
int main(int argc,char **argv) {
    if(argc!=2)return 2;mode=argv[1];snapshot=!strcmp(mode,"snapshot");
    if(!snapshot && strcmp(mode,"futex") && strcmp(mode,"ppoll") && strcmp(mode,"pselect") && strcmp(mode,"epoll") && strcmp(mode,"read"))return 2;
    shared=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_SHARED|MAP_ANONYMOUS,-1,0);
    if(shared==MAP_FAILED || pipe(control) || pipe(confirmed) || pipe(result_pipe) || pipe(data))return 2;
    shared->original=(struct timespec){600,0};
    ep=epoll_create1(0);struct epoll_event e={.events=EPOLLIN,.data.u64=0x5eed};
    if(ep<0 || epoll_ctl(ep,EPOLL_CTL_ADD,data[0],&e))return 2;
    pid_t child=fork();if(child<0)return 2;
    if(!child) {
        close(result_pipe[0]);unix_waiter_tid=gettid();
        pthread_t ct;if(pthread_create(&ct,NULL,controller,NULL))_exit(2);
        struct result r=wait_once();
        if(!strcmp(mode,"epoll") && r.value==-1 && r.error==EINTR) {
            /* Linux epoll itself returns EINTR after SIGSTOP/SIGCONT, even
             * without a handler. Preserve that native API result distinctly. */
            r=wait_once();r.stop_eintr=1;
        }
        if(write(result_pipe[1],&r,sizeof r)!=sizeof r)_exit(2);
        pthread_join(ct,NULL);_exit(0);
    }
    close(result_pipe[1]);observed(child);
    if(!snapshot) {
        siginfo_t si={0};
        if(kill(child,SIGSTOP) || waitid(P_PID,child,&si,WSTOPPED) || si.si_code!=CLD_STOPPED || si.si_status!=SIGSTOP)fail(__LINE__);
        if(kill(child,SIGCONT) || waitid(P_PID,child,&si,WCONTINUED) || si.si_code!=CLD_CONTINUED || si.si_status!=SIGCONT)fail(__LINE__);
        stopped=1;
        observed(child);
    } else {
        /* Separate processes + pipe release synchronize this deliberately
         * mutable public input AFTER the kernel has copied the timeout. */
        shared->original=(struct timespec){-1,0};
    }
    byte(control[1],'G');take(confirmed[0]);observed(child);
    if(!strcmp(mode,"futex") || snapshot) {
        __atomic_store_n(&shared->word,1,__ATOMIC_RELEASE);
        if(syscall(SYS_futex,&shared->word,FUTEX_WAKE,1,NULL,NULL,0)!=1)fail(__LINE__);
    } else byte(data[1],'W');
    struct result r;if(read(result_pipe[0],&r,sizeof r)!=sizeof r)fail(__LINE__);
    int status=0;if(waitpid(child,&status,0)!=child || !WIFEXITED(status) || WEXITSTATUS(status)) { fprintf(stderr,"wait child status %#x\n",status);fail(__LINE__); }
    long want=(!strcmp(mode,"futex") || snapshot)?0:1;
    if(r.value!=want || r.error || !r.payload || r.stop_eintr!=!strcmp(mode,"epoll")) { fprintf(stderr,"wait %s result %ld errno %d payload %d\n",mode,r.value,r.error,r.payload);return 1; }
    printf("OK %s: blocked, stopped=%d, resumed, completed=%ld, payload=1\n",mode,stopped,r.value);return 0;
}
