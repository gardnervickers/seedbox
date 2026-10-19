/* Controlled reproduction of failed_clone_vm:eagain-flush, Seedbox f32472b7,
 * DR cb47540b958684893b219a9faae31e99cb93f832. The external pids launcher
 * must attribute EAGAIN to its owned cgroup pids.events max increment. */
#define _GNU_SOURCE
#include "support.h"
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/mman.h>
#include <sys/wait.h>
__attribute__((visibility("default"))) volatile int unix_waiter_tid;
UNIX_SUSPEND_MARKER;
static atomic_int ready,release;
static void *filler(void *unused) {
    (void)unused;
    if(!__atomic_load_n(&unix_waiter_tid,__ATOMIC_ACQUIRE))__atomic_store_n(&unix_waiter_tid,gettid(),__ATOMIC_RELEASE);
    atomic_fetch_add(&ready,1);
    while(!atomic_load(&release))syscall(SYS_futex,&release,FUTEX_WAIT_PRIVATE,0,NULL,NULL,0);
    return (void *)7;
}
int main(void) {
    pthread_t owned[128],helper;int count=0,error=0;
    if(pthread_create(&helper,NULL,filler,NULL))return 2;
    while(atomic_load(&ready)<1)sched_yield();
    /* Confirm the pre-existing helper is alive and blocked before exhaustion. */
    unix_wait_syscall(getpid(),unix_waiter_tid,SYS_futex);
    for(;count<128;count++) {
        error=pthread_create(&owned[count],NULL,filler,NULL);
        if(error)break;
        while(atomic_load(&ready)<count+2)sched_yield();
    }
    if(error!=EAGAIN || count<1) { fprintf(stderr,"expected attributed pids EAGAIN after fillers, got %d with %d\n",error,count);return 1; }
    uint32_t *code=mmap(NULL,4096,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);if(code==MAP_FAILED)return 2;
    for(unsigned i=0;i<100;i++) { unix_emit(code,i);if(((unsigned (*)(void))code)()!=i)return 1; }
    /* Public suspension verifies no failed-creation bookkeeping blocks synch-all.
     * Choose a genuine filler TID, published by the helper before it parks. */
    /* The helper publishes below through its own gettid. */
    unix_request_suspend();
    atomic_store(&release,1);syscall(SYS_futex,&release,FUTEX_WAKE_PRIVATE,INT32_MAX,NULL,NULL,0);
    for(int i=0;i<count;i++){void *v=NULL;if(pthread_join(owned[i],&v)||v!=(void *)7)return 1;}
    void *v=NULL;if(pthread_join(helper,&v)||v!=(void *)7)return 1;
    atomic_store(&release,1);if(pthread_create(&helper,NULL,filler,NULL)||pthread_join(helper,&v)||v!=(void *)7)return 1;
    struct { uint64_t flags,pidfd,child_tid,parent_tid,exit_signal,stack,stack_size,tls,set_tid,set_tid_size,cgroup; } args={.exit_signal=SIGCHLD};
    long child=syscall(SYS_clone3,&args,sizeof args);if(!child)_exit(7);
    int st=0;if(child<0 || waitpid(child,&st,0)!=child || !WIFEXITED(st) || WEXITSTATUS(st)!=7)return 1;
    puts("OK eagain: fillers acknowledged, EAGAIN, flush 100/100, joined, thread recovered, clone3 exit 7");return 0;
}
