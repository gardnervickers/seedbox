/* Public asynchronous/flush restoration axis derived from cfz:t_rwrx alias
 * publication (Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e). This is a
 * separate state oracle, not an inference from the synchronous INT3 control.
 * A generated aliased loop remains live across64 qualified public suspend/
 * flush transactions and64 actual asynchronous signals. No app byte mutation,
 * elapsed assertion, readiness sleep, or private DR layout is involved. */
#define _GNU_SOURCE
#include "support.h"
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>
#if defined(__aarch64__)
#include "signal_records.h"
#endif
struct state {
    int release,ready;
    uint64_t progress,out_gpr,padding;
    uint64_t out_vector[2],in_vector[2];
};
_Static_assert(offsetof(struct state,progress)==8 && offsetof(struct state,out_gpr)==16 && offsetof(struct state,out_vector)==32 && offsetof(struct state,in_vector)==48,"application assembly ABI");
static struct state state;
static volatile int signals;
__attribute__((visibility("default"))) int unix_alias_transaction;
__attribute__((visibility("default"))) volatile int unix_waiter_tid;
__attribute__((visibility("default"))) void *unix_alias_rx,*unix_alias_loop_lo,*unix_alias_loop_hi;
__attribute__((visibility("default"))) size_t unix_alias_size;
UNIX_SUSPEND_MARKER;
extern const unsigned char alias_loop_start[],alias_loop_body[],alias_loop_tail[],alias_loop_end[];
#if defined(__x86_64__)
asm(".text\n.globl alias_loop_start\n.type alias_loop_start,@function\nalias_loop_start:\n"
    "mov $0x1234,%r8\nmovdqu 48(%rdi),%xmm0\ncmp %r8,%r8\nmovl $1,4(%rdi)\n"
    ".globl alias_loop_body\nalias_loop_body:\n"
    "mov 8(%rdi),%r10\nlea 1(%r10),%r10\nmov %r10,8(%rdi)\n"
    "cmpl $0,(%rdi)\nje alias_loop_body\n"
    ".globl alias_loop_tail\nalias_loop_tail:\n"
    "mov %r8,16(%rdi)\nmovdqu %xmm0,32(%rdi)\nmov $42,%eax\nret\n"
    ".globl alias_loop_end\nalias_loop_end:\n");
#else
asm(".text\n.p2align 2\n.globl alias_loop_start\n.type alias_loop_start,%function\nalias_loop_start:\n"
    "mov x9,#0x1234\nldr q0,[x0,#48]\nmov x10,#0x60000000\nmsr nzcv,x10\n"
    "mov w10,#1\nadd x11,x0,#4\nstlr w10,[x11]\n"
    ".globl alias_loop_body\nalias_loop_body:\n"
    "ldr x10,[x0,#8]\nadd x10,x10,#1\nstr x10,[x0,#8]\n"
    "ldr w10,[x0]\ncbz w10,alias_loop_body\n"
    ".globl alias_loop_tail\nalias_loop_tail:\n"
    "str x9,[x0,#16]\nstr q0,[x0,#32]\nmov w0,#42\nret\n"
    ".globl alias_loop_end\nalias_loop_end:\n");
#endif
static void hex_diagnostic(uint64_t mask,uint64_t pc,uint64_t gpr,uint64_t flags,uint64_t sp,const uint64_t vector[2]) {
    const char *labels[]={"alias async mismatch mask="," pc="," gpr="," flags="," sp="," v0lo="," v0hi="};
    const uint64_t values[]={mask,pc,gpr,flags,sp,vector[0],vector[1]};char line[256];size_t n=0;
    for(unsigned i=0;i<7;i++) {
        for(const char *p=labels[i];*p;p++)line[n++]=*p;
        for(int shift=60;shift>=0;shift-=4)line[n++]="0123456789abcdef"[(values[i]>>shift)&15];
    }
    line[n++]='\n';ssize_t written=write(2,line,n);(void)written;
}
static void handler(int sig,siginfo_t *info,void *context) {
    ucontext_t *uc=context;uintptr_t pc;uint64_t gpr,flags,sp,vector[2]={0};unsigned bad=0;
    if(sig!=SIGUSR1||info->si_code!=SI_TKILL||info->si_pid!=getpid())bad|=1;
#if defined(__x86_64__)
    pc=(uintptr_t)uc->uc_mcontext.gregs[REG_RIP];
    gpr=uc->uc_mcontext.gregs[REG_R8];sp=uc->uc_mcontext.gregs[REG_RSP];flags=uc->uc_mcontext.gregs[REG_EFL];
    if(gpr!=0x1234)bad|=2;if((sp&15)!=8)bad|=4;if((flags&0x8d5)!=0x44)bad|=8;
    if(!uc->uc_mcontext.fpregs)bad|=16;
    else memcpy(vector,&uc->uc_mcontext.fpregs->_xmm[0],16);
#else
    pc=uc->uc_mcontext.pc;
    gpr=uc->uc_mcontext.regs[9];sp=uc->uc_mcontext.sp;flags=uc->uc_mcontext.pstate;
    if(gpr!=0x1234)bad|=2;if(sp&15)bad|=4;if((flags&0xf0000000u)!=0x60000000u)bad|=8;
    uint64_t layout;int sve,za;char error[160];
    if(a64p_check_frame_records(uc,&layout,&sve,&za,error,sizeof error))bad|=16;
    struct a64p_fpsimd_rec *fp=(struct a64p_fpsimd_rec *)a64p_find_rec(uc,FPSIMD_MAGIC_);
    if(!fp)bad|=16;else memcpy(vector,&fp->v[0],16);
#endif
    if(memcmp(vector,state.in_vector,16))bad|=16;
    if(pc<(uintptr_t)unix_alias_loop_lo || pc>=(uintptr_t)unix_alias_loop_hi)bad|=32;
    if(sigismember(&uc->uc_sigmask,SIGWINCH)!=1 || sigismember(&uc->uc_sigmask,SIGUSR1)!=0)bad|=64;
    sigset_t current;if(sigprocmask(SIG_BLOCK,NULL,&current))bad|=128;
    else if(sigismember(&current,SIGUSR1)!=1 || sigismember(&current,SIGWINCH)!=1)bad|=128;
    if(bad) {hex_diagnostic(bad,pc,gpr,flags,sp,vector);_exit(3);}
    __atomic_add_fetch(&signals,1,__ATOMIC_RELEASE);
}
static void *worker(void *unused) {
    (void)unused;unix_waiter_tid=gettid();sigset_t mask;sigemptyset(&mask);sigaddset(&mask,SIGWINCH);
    if(pthread_sigmask(SIG_BLOCK,&mask,NULL))abort();
    int result=((int (*)(struct state *))unix_alias_rx)(&state);
    if(result!=42)abort();return NULL;
}
static void publish(unsigned char *w,unsigned char *x,size_t size) {
#if defined(__aarch64__)
    for(size_t i=0;i<size;i+=4)asm volatile("dc cvau,%0"::"r"(w+i):"memory");
    asm volatile("dsb ish":::"memory");
    for(size_t i=0;i<size;i+=4)asm volatile("ic ivau,%0"::"r"(x+i):"memory");
    asm volatile("dsb ish\nisb":::"memory");
#else
    (void)w;(void)x;(void)size;asm volatile("":::"memory");
#endif
}
int main(int argc,char **argv) {
    const char *mode=argc==1?"both":argc==2?argv[1]:"invalid";
    if(!strcmp(mode,"suspend"))unix_alias_transaction=1;
    else if(!strcmp(mode,"flush"))unix_alias_transaction=2;
    else if(strcmp(mode,"both")&&strcmp(mode,"signal"))return 2;
    long page=sysconf(_SC_PAGESIZE);if(page<4096)return 2;
    unix_alias_size=(size_t)(alias_loop_end-alias_loop_start);if(!unix_alias_size||unix_alias_size>(size_t)page)return 2;
    int fd=memfd_create("seedbox-alias-async",MFD_CLOEXEC);if(fd<0||ftruncate(fd,page))return 2;
    unsigned char *w=mmap(NULL,(size_t)page,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0),
        *x=mmap(NULL,(size_t)page,PROT_READ|PROT_EXEC,MAP_SHARED,fd,0);
    if(w==MAP_FAILED||x==MAP_FAILED)return 2;
    memcpy(w,alias_loop_start,unix_alias_size);publish(w,x,unix_alias_size);
    unix_alias_rx=x;unix_alias_loop_lo=x+(alias_loop_body-alias_loop_start);unix_alias_loop_hi=x+(alias_loop_tail-alias_loop_start);
    state.in_vector[0]=UINT64_C(0x0123456789abcdef);state.in_vector[1]=UINT64_C(0xfedcba9876543210);
    struct sigaction action={0};action.sa_sigaction=handler;action.sa_flags=SA_SIGINFO|SA_RESTART;sigemptyset(&action.sa_mask);
    if(sigaction(SIGUSR1,&action,NULL))return 2;
    pthread_t thread;if(pthread_create(&thread,NULL,worker,NULL))return 2;
    while(!__atomic_load_n(&state.ready,__ATOMIC_ACQUIRE))sched_yield();
    for(int round=1;round<=64;round++) {
        if(strcmp(mode,"signal"))unix_request_suspend();
        if(syscall(SYS_tgkill,getpid(),unix_waiter_tid,SIGUSR1))abort();
        while(__atomic_load_n(&signals,__ATOMIC_ACQUIRE)<round)sched_yield();
        /* A subsequent generated-loop store positively proves sigreturn and
         * restored execution; handler acknowledgement alone is insufficient. */
        uint64_t before=__atomic_load_n(&state.progress,__ATOMIC_ACQUIRE);
        while(__atomic_load_n(&state.progress,__ATOMIC_ACQUIRE)<=before)sched_yield();
    }
    __atomic_store_n(&state.release,1,__ATOMIC_RELEASE);if(pthread_join(thread,NULL))return 2;
    if(state.out_gpr!=0x1234||memcmp(state.out_vector,state.in_vector,16)||signals!=64)return 1;
    if(munmap(w,(size_t)page)||munmap(x,(size_t)page)||close(fd))return 2;
    puts("OK alias async: signals 64 GPR vector flags PC mask sigreturn progress preserved");return 0;
}
