/* Public port of DR 356c1821 suite/tests/client-interface/timer.{c,dll.c,templatex}.
 * Original 1,000 x 1,000 indirect-work population retained; source pacing sleeps
 * replaced by positive app/client/app notification state and sampler receipt. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
__attribute__((visibility("default"))) volatile int unix_timer_stage,unix_timer_sample;
static volatile sig_atomic_t ticks;
static void handler(int sig) { if(sig!=SIGALRM)_Exit(2);ticks++;if(!unix_timer_stage)unix_timer_stage=1;else if(unix_timer_stage==2)unix_timer_stage=3; }
__attribute__((noinline)) static int func1(int x) { return x>0?4*x:x/4; }
__attribute__((noinline)) static int func2(int x) { return x<0?4*x:x/4; }
static int (*volatile functions[2])(int)={func1,func2};
int main(int argc,char **argv) {
    int client=argc==2 && !strcmp(argv[1],"client");
    if(argc!=2 || (!client && strcmp(argv[1],"app")))return 2;
    struct sigaction a={0};a.sa_handler=handler;sigemptyset(&a.sa_mask);if(sigaction(SIGALRM,&a,NULL))return 2;
    struct itimerval timer={{0,10000},{0,10000}};if(setitimer(ITIMER_REAL,&timer,NULL))return 2;
    volatile uint64_t sum=0;
    for(int i=0;i<1000;i++)for(int j=0;j<1000;j++)sum+=(unsigned)functions[i<j](j)+(unsigned)functions[i&1](i*6);
    /* No finite work count substitutes for notification: completion requires
     * actual timer events, and the public client requires a live sampled PC. */
    while(client ? (unix_timer_stage!=3 || !unix_timer_sample) : ticks<3) {
        sum+=(unsigned)functions[sum&1]((int)(sum&0x7fff));
    }
    timer=(struct itimerval){{0,0},{0,0}};if(setitimer(ITIMER_REAL,&timer,NULL))return 2;
    puts("OK timer: application notifications preserved");return 0;
}
