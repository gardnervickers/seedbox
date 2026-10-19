/* Source origin: Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/probes/c/cfz.c:t_rwrx. Preserve its 5000 updates,64 slots,
 * 16-byte stride,uint16(i*7+1),sum87487500 and zero-stale oracle.
 * Lifetime controls were authored with the public Linux mapping API while
 * extracting the regression (published DR correctness change3d5298b8).
 * This tests DEFAULT hardware-cache-consistency instrumentation. It does not
 * qualify the source no_hw_cache_consistency profile or Seedbox progress client.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>
#define CHECK(c) do { if(!(c)) { fprintf(stderr,"alias line %d: %s (errno %d)\n",__LINE__,#c,errno);exit(1); } } while(0)
static size_t page;
static int batches;
#if defined(__x86_64__)
static int rip_literal;
#endif
static volatile sig_atomic_t expected,traps;
static volatile uintptr_t trap_pc;
static void emit(unsigned char *w,unsigned value,int with_trap) {
#if defined(__x86_64__)
    if(rip_literal) {
        /* Additional source-derived x86 data-relative axis, not the original
         * immediate oracle and not an invented ARM RIP equivalent. */
        CHECK(!with_trap);w[0]=0x8b;w[1]=0x05;
        uint32_t displacement=2;memcpy(w+2,&displacement,4);w[6]=0xc3;
        memcpy(w+8,&value,4);return;
    }
    /* A coherent x86 publication has no cache-maintenance instruction/syscall.
     * Each update completes before this single writer's consumer executes. */
    w[0]=0xb8;memcpy(w+1,&value,4);w[5]=with_trap?0xcc:0xc3;
    if(with_trap)w[6]=0xc3;
#elif defined(__aarch64__)
    uint32_t code[3]={0x52800000u | (uint32_t)(uint16_t)value<<5,
                     with_trap?0xd4200000u:0xd65f03c0u,0xd65f03c0u};
    memcpy(w,code,with_trap?12:8);
#else
#error Linux x86-64 or AArch64 required
#endif
}
static void publish(unsigned char *w,unsigned char *x) {
#if defined(__aarch64__)
    /* Source publication: clean writable alias to PoU, invalidate executable
     * alias, with the required ordering. Address-by-four covers every byte of
     * the at-most12-byte sequence independent of the hardware cache line size. */
    for(int i=0;i<12;i+=4)asm volatile("dc cvau,%0"::"r"(w+i):"memory");
    asm volatile("dsb ish":::"memory");
    for(int i=0;i<12;i+=4)asm volatile("ic ivau,%0"::"r"(x+i):"memory");
    asm volatile("dsb ish\nisb":::"memory");
#else
    (void)w;(void)x;asm volatile("":::"memory");
#endif
}
static int invoke(unsigned char *x) { return ((int (*)(void))x)(); }
static int backing(void) {
    int fd=memfd_create("seedbox-alias",MFD_CLOEXEC);
    CHECK(fd>=0 && ftruncate(fd,(off_t)(2*page))==0);return fd;
}
static unsigned char *mapping(int fd,int prot,int flags,size_t off) {
    void *p=mmap(NULL,page,prot,flags,fd,(off_t)off);CHECK(p!=MAP_FAILED);return p;
}
static void set(unsigned char *w,unsigned char *x,unsigned value) {
    emit(w,value,0);publish(w,x);CHECK(invoke(x)==(int)value);
}
static void trap(int sig,siginfo_t *si,void *context) {
    ucontext_t *uc=context;int ok=sig==SIGTRAP;
#if defined(__x86_64__)
    ok &= uc->uc_mcontext.gregs[REG_RAX]==expected &&
          (uintptr_t)uc->uc_mcontext.gregs[REG_RIP]==trap_pc && si->si_code==SI_KERNEL;
#else
    ok &= uc->uc_mcontext.regs[0]==(unsigned)expected &&
          uc->uc_mcontext.pc==trap_pc && si->si_code==TRAP_BRKPT &&
          (uintptr_t)si->si_addr==trap_pc && !(uc->uc_mcontext.sp&15);
    uc->uc_mcontext.pc+=4; /* A64 BRK saves the trapping PC; x86 INT3 saves next PC. */
#endif
    if(!ok)_Exit(3);
    traps++;
}
static void oracle(unsigned char *w,unsigned char *x,int with_trap) {
    long sum=0,stale=0;
    for(long i=0;i<5000;i++) {
        unsigned value=(uint16_t)(i*7+1);size_t slot=(size_t)(i%64)*16;
        emit(w+slot,value,with_trap);expected=(sig_atomic_t)value;
#if defined(__x86_64__)
        trap_pc=(uintptr_t)(x+slot+6);
#else
        trap_pc=(uintptr_t)(x+slot+4);
#endif
        publish(w+slot,x+slot);int got=invoke(x+slot);sum+=got;stale+=got!=(int)value;
    }
    /* Print actual failure counts; never replace this with an unconditional OK. */
    if(sum!=87487500 || stale)fprintf(stderr,"alias sum %ld stale %ld\n",sum,stale);
    CHECK(sum==87487500 && stale==0);batches++;
}
static void byte_write(int fd) { char c=1;ssize_t n;do n=write(fd,&c,1);while(n<0 && errno==EINTR);CHECK(n==1); }
static void byte_read(int fd) { char c=0;ssize_t n;do n=read(fd,&c,1);while(n<0 && errno==EINTR);CHECK(n==1 && c==1); }
static void process_oracle(unsigned char *w,unsigned char *x) {
    int ready[2],consumed[2];CHECK(pipe2(ready,O_CLOEXEC)==0 && pipe2(consumed,O_CLOEXEC)==0);
    set(w,x,1);pid_t child=fork();CHECK(child>=0);
    if(!child) {
        CHECK(close(ready[0])==0 && close(consumed[1])==0);
        for(long i=0;i<5000;i++) {
            size_t slot=(size_t)(i%64)*16;emit(w+slot,(uint16_t)(i*7+1),0);publish(w+slot,x+slot);
            byte_write(ready[1]);byte_read(consumed[0]);
        }
        _exit(0);
    }
    CHECK(close(ready[1])==0 && close(consumed[0])==0);long sum=0,stale=0;
    for(long i=0;i<5000;i++) {
        byte_read(ready[0]);
#if defined(__aarch64__)
        /* The writer's broadcast I-cache maintenance precedes its pipe receipt.
         * Synchronize this consumer's instruction stream after receiving it. */
        asm volatile("isb":::"memory");
#endif
        int got=invoke(x+(size_t)(i%64)*16);sum+=got;stale+=got!=(uint16_t)(i*7+1);
        byte_write(consumed[1]);
    }
    int status;pid_t reaped;do reaped=waitpid(child,&status,0);while(reaped<0 && errno==EINTR);
    CHECK(reaped==child && WIFEXITED(status) && WEXITSTATUS(status)==0);
    CHECK(sum==87487500 && stale==0);batches++;
    CHECK(close(ready[0])==0 && close(consumed[1])==0);
}
int main(int argc,char **argv) {
    CHECK(argc==2);const char *mode=argv[1];long size=sysconf(_SC_PAGESIZE);CHECK(size>=4096);page=(size_t)size;
    int fd=backing();unsigned char *w=mapping(fd,PROT_READ|PROT_WRITE,MAP_SHARED,0),
        *x=mapping(fd,PROT_READ|PROT_EXEC,MAP_SHARED,0);
#if defined(__x86_64__)
    if(!strcmp(mode,"rip-literal")){rip_literal=1;oracle(w,x,0);}
    else
#endif
    if(!strcmp(mode,"boundary"))oracle(w,x,0);
    else if(!strcmp(mode,"closed")) {CHECK(close(fd)==0);fd=-1;oracle(w,x,0);}
    else if(!strcmp(mode,"late")) {set(w,x,1);CHECK(munmap(w,page)==0);w=mapping(fd,PROT_READ|PROT_WRITE,MAP_SHARED,0);oracle(w,x,0);}
    else if(!strcmp(mode,"protect")) {
        set(w,x,1);CHECK(mprotect(w,page,PROT_READ)==0 && mprotect(w,page,PROT_READ|PROT_WRITE)==0);
        CHECK(mprotect(x,page,PROT_READ)==0 && mprotect(x,page,PROT_READ|PROT_EXEC)==0);oracle(w,x,0);
    } else if(!strcmp(mode,"offset")) {
        unsigned char *w2=mapping(fd,PROT_READ|PROT_WRITE,MAP_SHARED,page),*x2=mapping(fd,PROT_READ|PROT_EXEC,MAP_SHARED,page);
        set(w2,x2,29);oracle(w,x,0);CHECK(invoke(x2)==29);CHECK(munmap(w2,page)==0 && munmap(x2,page)==0);
    } else if(!strcmp(mode,"reuse")) {
        set(w,x,31);CHECK(close(fd)==0);int other=backing();CHECK(other==fd);
        unsigned char *w2=mapping(other,PROT_READ|PROT_WRITE,MAP_SHARED,0),*x2=mapping(other,PROT_READ|PROT_EXEC,MAP_SHARED,0);
        oracle(w2,x2,0);CHECK(invoke(x)==31);oracle(w,x,0);CHECK(munmap(w2,page)==0 && munmap(x2,page)==0);
    } else if(!strcmp(mode,"remap")) {
        set(w,x,1);void *dest=mmap(NULL,page,PROT_NONE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);CHECK(dest!=MAP_FAILED);
        void *moved=mremap(x,page,page,MREMAP_MAYMOVE|MREMAP_FIXED,dest);CHECK(moved==dest);x=moved;
        dest=mmap(NULL,page,PROT_NONE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);CHECK(dest!=MAP_FAILED);
        moved=mremap(w,page,page,MREMAP_MAYMOVE|MREMAP_FIXED,dest);CHECK(moved==dest);w=moved;oracle(w,x,0);
    } else if(!strcmp(mode,"replace")) {
        set(w,x,1);int other=backing();unsigned char *w2=mapping(other,PROT_READ|PROT_WRITE,MAP_SHARED,0);emit(w2,19,0);
        CHECK(mmap(x,page,PROT_READ|PROT_EXEC,MAP_SHARED|MAP_FIXED,other,0)==x);publish(w2,x);CHECK(invoke(x)==19);
        oracle(w2,x,0);CHECK(munmap(w2,page)==0 && close(other)==0);
    } else if(!strcmp(mode,"stable")) {set(w,x,1);for(int i=0;i<20000;i++)CHECK(invoke(x)==1);oracle(w,x,0);}
    else if(!strcmp(mode,"private")) {
        set(w,x,1);unsigned char *cow=mapping(fd,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE,0);
        set(cow,cow,19);oracle(w,x,0);CHECK(invoke(cow)==19);CHECK(munmap(cow,page)==0);
    } else if(!strcmp(mode,"mixed")) {
        void *area=mmap(NULL,2*page,PROT_NONE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);CHECK(area!=MAP_FAILED);
        unsigned char *private=mmap(area,page,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS|MAP_FIXED,-1,0);CHECK(private==area);emit(private,19,0);
        unsigned char *shared=mmap(private+page,page,PROT_READ|PROT_EXEC,MAP_SHARED|MAP_FIXED,fd,0);CHECK(shared==private+page);
        CHECK(mprotect(private,2*page,PROT_READ|PROT_EXEC)==0);publish(private,private);CHECK(invoke(private)==19);
        oracle(w,shared,0);CHECK(invoke(private)==19);CHECK(munmap(private,2*page)==0);
    } else if(!strcmp(mode,"process"))process_oracle(w,x);
    else if(!strcmp(mode,"signal")) {
        struct sigaction sa={0};sa.sa_sigaction=trap;sa.sa_flags=SA_SIGINFO;sigemptyset(&sa.sa_mask);CHECK(sigaction(SIGTRAP,&sa,NULL)==0);
        oracle(w,x,1);CHECK(traps==5000);
    } else CHECK(0);
    CHECK(munmap(w,page)==0 && munmap(x,page)==0);if(fd>=0)CHECK(close(fd)==0);
    printf("OK alias %s: batches %d updates 5000 slots 64 stride 16 sum 87487500 stale 0 traps %d\n",mode,batches,(int)traps);
    return 0;
}
