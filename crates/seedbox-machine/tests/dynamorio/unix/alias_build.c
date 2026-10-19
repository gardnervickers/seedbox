/* Public application port of the deterministic process-writer reproducer in
 * /tmp/dr-alias-review-r1-probe, discovered while reviewing source adaptation
 * 3d5298b8. Source scenario: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e
 * tools/jvm-fuzz/probes/c/cfz.c:t_rwrx. Original5000/64 ordinary publication
 * oracle remains in aliases.c; this distinct case covers publication DURING
 * construction of a translated branch. No DR private layout is inspected.
 * Bytes64..88 are this application's own protocol, consumed via dr_safe_read.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#define CHECK(c) do {if(!(c)){fprintf(stderr,"alias-build line %d: %s errno%d\n",__LINE__,#c,errno);exit(2);}}while(0)
static void publish(unsigned char *w,unsigned char *x) {
#if defined(__aarch64__)
    for(int i=0;i<40;i+=4)asm volatile("dc cvau,%0"::"r"(w+i):"memory");
    asm volatile("dsb ish":::"memory");
    for(int i=0;i<40;i+=4)asm volatile("ic ivau,%0"::"r"(x+i):"memory");
    asm volatile("dsb ish\nisb":::"memory");
#else
    (void)w;(void)x;asm volatile("":::"memory");
#endif
}
static void branch(unsigned char *w,int target) {
#if defined(__x86_64__)
    uint32_t displacement=(uint32_t)(target-5);memcpy(w+1,&displacement,4);
#else
    uint32_t instruction=0x14000000u | (unsigned)target/4;memcpy(w,&instruction,4);
#endif
}
static void value(unsigned char *w,int result) {
#if defined(__x86_64__)
    w[0]=0xb8;memcpy(w+1,&result,4);w[5]=0xc3;
#else
    uint32_t code[2]={0x52800000u | (unsigned)result<<5,0xd65f03c0u};memcpy(w,code,sizeof code);
#endif
}
static void send(int fd) {char c=1;ssize_t n;do n=write(fd,&c,1);while(n<0&&errno==EINTR);CHECK(n==1);}
static void receive(int fd) {char c=0;ssize_t n;do n=read(fd,&c,1);while(n<0&&errno==EINTR);CHECK(n==1&&c==1);}
int main(void) {
    long page=sysconf(_SC_PAGESIZE);CHECK(page>=4096);
    int fd=memfd_create("seedbox-alias-build",0);CHECK(fd>=0&&ftruncate(fd,page)==0);
    unsigned char *w=mmap(NULL,(size_t)page,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0),
        *x=mmap(NULL,(size_t)page,PROT_READ|PROT_EXEC,MAP_SHARED,fd,0);
    CHECK(w!=MAP_FAILED&&x!=MAP_FAILED);
#if defined(__x86_64__)
    w[0]=0xe9;
#endif
    branch(w,16);value(w+16,11);value(w+32,22);
    const uint64_t magic=UINT64_C(0xabbaccdd12344321);memcpy(w+64,&magic,sizeof magic);
    int go[2],ack[2];CHECK(pipe(go)==0&&pipe(ack)==0);
    memcpy(w+80,&go[1],sizeof(int));memcpy(w+84,&ack[0],sizeof(int));publish(w,x);
    pid_t child=fork();CHECK(child>=0);
    if(!child) {
        CHECK(close(go[1])==0&&close(ack[0])==0);
        receive(go[0]);branch(w,32);publish(w,x);
        __atomic_store_n(w+88,1,__ATOMIC_RELEASE);send(ack[1]);_exit(0);
    }
    CHECK(close(go[0])==0&&close(ack[1])==0);
    int first=((int (*)(void))x)();
    /* Native/plain controls release the publisher after the first call. The
     * public BB client releases it after decode and awaits the same receipt.
     * This flag records an actual publication, not an inference from silence. */
    if(!__atomic_load_n(w+88,__ATOMIC_ACQUIRE)){send(go[1]);receive(ack[0]);}
    int status;pid_t reaped;do reaped=waitpid(child,&status,0);while(reaped<0&&errno==EINTR);
    CHECK(reaped==child&&WIFEXITED(status)&&WEXITSTATUS(status)==0);
    CHECK(__atomic_load_n(w+88,__ATOMIC_ACQUIRE)==1);
    /* An app-owned publication after joining makes final bytes unambiguous in
     * every profile. No writer exists during the following1000 calls. */
    branch(w,32);publish(w,x);int stale=0;
    for(int i=0;i<1000;i++)stale+=((int (*)(void))x)()!=22;
    fprintf(stderr,"alias-build: first=%d (diagnostic only)\n",first);
    printf("alias-build: publisher joined 1 calls 1000 expected 22 stale %d\n",stale);
    CHECK(close(go[1])==0&&close(ack[0])==0&&close(fd)==0);
    CHECK(munmap(w,(size_t)page)==0&&munmap(x,(size_t)page)==0);
    return stale?1:0;
}
