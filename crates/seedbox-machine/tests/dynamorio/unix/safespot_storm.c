/* Source: Seedbox f32472b7 tools/jvm-fuzz/repros/safespot_storm.c.
 * Keep concurrent writers/executors/signals/map-execute-unmap; source's timed
 * duration/pacing becomes 2000 acknowledged signals plus positive work floors. */
#define _GNU_SOURCE
#include "support.h"
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdatomic.h>
#include <sys/mman.h>
static uint32_t *code;
static atomic_int stop,ready;
static atomic_long written[2],executed[2],mapped;
static volatile pid_t tids[2];
static sem_t handled;
static void handler(int sig) { if(sig!=SIGUSR1)_exit(2);sem_post(&handled); }
static void *writer(void *p) {
    long k=(long)p;atomic_fetch_or(&ready,1<<k);
    for(unsigned i=0;!atomic_load(&stop);i++) { unix_emit(code+((i*2+k)%64)*4,i&0xffff);atomic_fetch_add(&written[k],1); }
    return NULL;
}
static void *executor(void *p) {
    long k=(long)p;tids[k]=gettid();atomic_fetch_or(&ready,1<<(2+k));
    for(unsigned i=0;!atomic_load(&stop);i++) { ((int (*)(void))(code+(i%64)*4))();atomic_fetch_add(&executed[k],1); }
    return NULL;
}
static void *signaler(void *p) {
    (void)p;atomic_fetch_or(&ready,1<<4);while(atomic_load(&ready)!=63)sched_yield();
    for(unsigned i=0;i<2000;i++) {
        if(syscall(SYS_tgkill,getpid(),tids[i&1],SIGUSR1))abort();
        while(sem_wait(&handled) && errno==EINTR){}
    }
    return NULL;
}
static void *mapper(void *p) {
    (void)p;atomic_fetch_or(&ready,1<<5);
    for(unsigned i=0;!atomic_load(&stop);i++) {
        uint32_t *m=mmap(NULL,4096,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);if(m==MAP_FAILED)abort();
        unix_emit(m,i&0xffff);if(((unsigned (*)(void))m)()!=(i&0xffff))abort();munmap(m,4096);atomic_fetch_add(&mapped,1);
    }
    return NULL;
}
int main(void) {
    sem_init(&handled,0,0);struct sigaction a={0};a.sa_handler=handler;a.sa_flags=SA_RESTART;sigemptyset(&a.sa_mask);if(sigaction(SIGUSR1,&a,NULL))return 2;
    code=mmap(NULL,4096,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);if(code==MAP_FAILED)return 2;
    for(unsigned i=0;i<64;i++)unix_emit(code+i*4,i);
    pthread_t t[6];if(pthread_create(&t[0],NULL,writer,(void *)0)||pthread_create(&t[1],NULL,writer,(void *)1)||
        pthread_create(&t[2],NULL,executor,(void *)0)||pthread_create(&t[3],NULL,executor,(void *)1)||
        pthread_create(&t[4],NULL,signaler,NULL)||pthread_create(&t[5],NULL,mapper,NULL))return 2;
    pthread_join(t[4],NULL);
    while(atomic_load(&mapped)<1000 || !atomic_load(&written[0]) || !atomic_load(&written[1]) || !atomic_load(&executed[0]) || !atomic_load(&executed[1]))sched_yield();
    atomic_store(&stop,1);for(int i=0;i<6;i++)if(i!=4)pthread_join(t[i],NULL);
    puts("OK safespot: 2000 signals acknowledged, two writers, two executors, 1000 mappings");return 0;
}
