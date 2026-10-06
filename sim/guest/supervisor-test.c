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
    /* Real rootless Unix socket -> supervisor -> management framing and reply. */
    char directory[]="/tmp/seedbox-relay-XXXXXX";assert(mkdtemp(directory));
    struct sockaddr_un address={.sun_family=AF_UNIX};
    assert(snprintf(address.sun_path,sizeof(address.sun_path),"%s/control.sock",directory)>0);
    control_listener=socket(AF_UNIX,SOCK_STREAM|SOCK_NONBLOCK|SOCK_CLOEXEC,0);assert(control_listener>=0);
    assert(!bind(control_listener,(struct sockaddr*)&address,sizeof(address)));assert(!listen(control_listener,1));
    int peer=socket(AF_UNIX,SOCK_STREAM,0);assert(peer>=0);assert(!connect(peer,(struct sockaddr*)&address,sizeof(address)));
    const char *request="{\"version\":1,\"id\":7,\"op\":\"now\"}";
    assert(write(peer,request,strlen(request))==(ssize_t)strlen(request));relay_requests();assert(!waiting_relay);
    assert(write(peer,"\n",1)==1);relay_requests();assert(waiting_relay==1);
    n=read(sockets[1],buffer,sizeof(buffer)-1);assert(n>0);buffer[n]=0;
    assert(!strcmp(buffer,"SBX_RPC1 1 {\"version\":1,\"id\":7,\"op\":\"now\"}\n"));
    char reply[]="SBX_REPLY1 1 {\"version\":1,\"id\":7,\"ok\":true}";control(reply,&child);assert(!waiting_relay);
    n=read(peer,buffer,sizeof(buffer)-1);assert(n>0);buffer[n]=0;assert(!strcmp(buffer,"{\"version\":1,\"id\":7,\"ok\":true}\n"));
    /* A stale reply must not be delivered to a later call. */
    pid_t check=fork();assert(check>=0);if(!check){control(reply,&child);_exit(0);}
    assert(waitpid(check,&status,0)==check);assert(WIFEXITED(status)&&WEXITSTATUS(status)==125);
    close(peer);relay_requests();assert(control_client==-1);assert(control_connected);
    unlink(address.sun_path);rmdir(directory);
}
