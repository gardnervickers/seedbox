/* Actual sleep completion portions of source cfz.c t_restart.stopContinue.
 * Positive blocked-wait observation + SIGSTOP/CONT receipts replace parent
 * pacing. Completion checks the syscall result; original elapsed-duration
 * claims remain explicitly unresolved and are not asserted here. */
#define _GNU_SOURCE
#include "support.h"
#include <signal.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
int main(int argc,char **argv) {
    if(argc!=2)return 2;int absolute=!strcmp(argv[1],"absolute");
    if(!absolute && strcmp(argv[1],"relative"))return 2;
    pid_t child=fork();if(child<0)return 2;
    if(!child) {
        struct timespec request={1,0};long result;
        if(absolute) {
            clock_gettime(CLOCK_MONOTONIC,&request);request.tv_sec++;
            /* seedbox-syscall-subject: clock_nanosleep; bound-ms=1000; invariant=positive blocked-entry then acknowledged stop/continue completes with result zero. */
            result=syscall(SYS_clock_nanosleep,CLOCK_MONOTONIC,TIMER_ABSTIME,&request,NULL);
        } else {
            /* seedbox-syscall-subject: nanosleep; bound-ms=1000; invariant=positive blocked-entry then acknowledged stop/continue completes with result zero. */
            result=syscall(SYS_nanosleep,&request,NULL);
        }
        _exit(result!=0);
    }
    unix_wait_syscall(child,child,absolute?SYS_clock_nanosleep:SYS_nanosleep);
    siginfo_t info={0};
    if(kill(child,SIGSTOP) || waitid(P_PID,child,&info,WSTOPPED) || info.si_code!=CLD_STOPPED || info.si_status!=SIGSTOP)return 1;
    if(kill(child,SIGCONT) || waitid(P_PID,child,&info,WCONTINUED) || info.si_code!=CLD_CONTINUED || info.si_status!=SIGCONT)return 1;
    int status;if(waitpid(child,&status,0)!=child || !WIFEXITED(status) || WEXITSTATUS(status))return 1;
    printf("OK stop sleep %s: blocked, stopped, continued, result 0\n",argv[1]);return 0;
}
