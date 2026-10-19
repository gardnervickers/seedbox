/* Seedbox f32472b7 tools/jvm-fuzz/repros/pid1_raise_dfl.c; DR fixes
 * 99333d6867a29f0d803d42203f5112f71963c544 and 4c576e69b47a52cb4b41067e57018c2048b77247.
 * Launched as the actual namespace init; a resident init wrapper is invalid. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <unistd.h>
static volatile sig_atomic_t got;
static void caught(int sig) { got=sig; }
int main(int argc,char **argv) {
    int outside=argc==2 && !strcmp(argv[1],"non-init-default");
    if(argc!=2 || (outside ? getpid()==1 : getpid()!=1)) { fputs("PID1 fixture must be actual namespace init\n",stderr);return 2; }
    struct rlimit core={0,0};if(setrlimit(RLIMIT_CORE,&core))return 2;
    sigset_t all,one;sigfillset(&all);sigprocmask(SIG_BLOCK,&all,NULL);
    sigemptyset(&one);
    if(outside || !strcmp(argv[1],"self-default")) {
        signal(SIGSEGV,SIG_DFL);sigaddset(&one,SIGSEGV);sigprocmask(SIG_UNBLOCK,&one,NULL);
        if(raise(SIGSEGV))return 2;if(outside)return 1;puts("OK pid1: still alive after raise(SIGSEGV)");return 0;
    }
    if(!strcmp(argv[1],"caught")) {
        signal(SIGUSR1,caught);sigaddset(&one,SIGUSR1);sigprocmask(SIG_UNBLOCK,&one,NULL);
        if(raise(SIGUSR1) || got!=SIGUSR1)return 1;puts("OK pid1: handler received SIGUSR1");return 0;
    }
    sigemptyset(&all);sigprocmask(SIG_SETMASK,&all,NULL);
    if(!strcmp(argv[1],"segv")) {
        void *page=mmap(NULL,4096,PROT_NONE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
        if(page==MAP_FAILED)return 2;((void (*)(void))page)();
    } else if(!strcmp(argv[1],"ill")) {
#if defined(__aarch64__)
        asm volatile(".inst 0x00000000");
#else
        asm volatile("ud2");
#endif
    } else if(!strcmp(argv[1],"trap")) {
#if defined(__aarch64__)
        asm volatile("brk #0");
#else
        asm volatile("int3");
#endif
    } else return 2;
    return 1;
}
