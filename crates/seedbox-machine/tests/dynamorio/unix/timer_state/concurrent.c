#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>
#define N 128
static pthread_barrier_t gate;
static long previous[2][N];
static int timer_kind;
#define CHECK(x) do { if (!(x)) { fprintf(stderr,"line=%d errno=%d\n",__LINE__,errno); abort(); } } while (0)
static void *worker(void *arg) {
    intptr_t id=(intptr_t)arg;
    int r=pthread_barrier_wait(&gate); CHECK(r==0 || r==PTHREAD_BARRIER_SERIAL_THREAD);
    for(int i=0;i<N;i++) {
        if(id==2) {
            struct itimerval bad={{0,1000000},{0,0}};
            errno=0; CHECK(syscall(SYS_setitimer,timer_kind,&bad,NULL)==-1 && errno==EINVAL);
            errno=0; CHECK(syscall(SYS_setitimer,timer_kind,(void *)1,NULL)==-1 && errno==EFAULT);
        } else {
            struct itimerval set={{id*N+i+1,0},{0,0}},old;
            CHECK(setitimer(timer_kind,&set,&old)==0);
            CHECK(old.it_value.tv_sec==0 && old.it_value.tv_usec==0 && old.it_interval.tv_usec==0);
            previous[id][i]=old.it_interval.tv_sec;
        }
        struct itimerval got;
        CHECK(getitimer(timer_kind,&got)==0);
        CHECK(got.it_value.tv_sec==0 && got.it_value.tv_usec==0);
        CHECK(got.it_interval.tv_usec==0 && got.it_interval.tv_sec>=0 && got.it_interval.tv_sec<=2*N);
    }
    return NULL;
}
int main(void) {
    for(timer_kind=0;timer_kind<3;timer_kind++) {
        pthread_t threads[3];
        struct itimerval zero={{0,0},{0,0}},last;
        CHECK(setitimer(timer_kind,&zero,NULL)==0);
        CHECK(pthread_barrier_init(&gate,NULL,3)==0);
        for(int i=0;i<3;i++) CHECK(pthread_create(&threads[i],NULL,worker,(void *)(intptr_t)i)==0);
        for(int i=0;i<3;i++) CHECK(pthread_join(threads[i],NULL)==0);
        CHECK(pthread_barrier_destroy(&gate)==0);
        CHECK(getitimer(timer_kind,&last)==0);
        int seen[2*N+1]={0};
        CHECK(last.it_value.tv_sec==0 && last.it_value.tv_usec==0 && last.it_interval.tv_usec==0);
        CHECK(last.it_interval.tv_sec>=0 && last.it_interval.tv_sec<=2*N);
        seen[last.it_interval.tv_sec]++;
        for(int t=0;t<2;t++) for(int i=0;i<N;i++) {
            CHECK(previous[t][i]>=0 && previous[t][i]<=2*N);
            seen[previous[t][i]]++;
        }
        if(timer_kind==ITIMER_REAL) CHECK(seen[0]==2*N+1);
        else for(int i=0;i<=2*N;i++) CHECK(seen[i]==1);
        CHECK(setitimer(timer_kind,&zero,NULL)==0);
        printf("kind=%d writes=%d rejected=%d queries=%d old-value-chain=PASS\n",timer_kind,2*N,2*N,3*N+1);
    }
    puts("PASS concurrent timer transactions");
    return 0;
}
