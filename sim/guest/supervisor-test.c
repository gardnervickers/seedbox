#define SEEDBOX_SUPERVISOR_TEST
#include "compose-init.c"
#include <assert.h>
#include <sys/socket.h>
int main(int argc, char **argv) {
    if (argc > 1) { for (;;) pause(); }
    int sockets[2]; assert(!socketpair(AF_UNIX,SOCK_STREAM,0,sockets));
    management = sockets[0];
    environment = (char *[]){"PATH=/bin:/usr/bin",NULL}; user=""; working_dir="/";
    arguments = (char *[]){argv[0],"child",NULL};
    pid_t child = start(); assert(child > 0); pid_t original = child;
    char stop_command[]="stop 1"; control(stop_command,&child); assert(child == -1);
    int status; assert(waitpid(original,&status,WNOHANG) == -1 && errno == ECHILD);
    char buffer[512]; ssize_t n = read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0;
    assert(strstr(buffer,"SEEDBOX_EFFECT id=1 action=stop guest_ns="));
    char repeated[]="stop 2"; control(repeated,&child);
    n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0; assert(strstr(buffer,"SEEDBOX_FAILURE id=2"));
    char restart[]="start 3"; control(restart,&child); assert(child>0);
    n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0; assert(strstr(buffer,"SEEDBOX_EFFECT id=3 action=start"));
    assert(!stop(&child));
    arguments=(char *[]){"/definitely/no/such/executable",NULL};
    char failed[]="start 4"; control(failed,&child); assert(child<0);
    n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0; assert(strstr(buffer,"SEEDBOX_FAILURE id=4"));
    assert(waitpid(-1,&status,WNOHANG)==-1 && errno==ECHILD);
}
