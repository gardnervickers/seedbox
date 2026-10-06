#define _GNU_SOURCE
#include <stdint.h>
static int readiness_ioperm(unsigned long first, unsigned long count, int enabled);
static void readiness_outl(uint32_t id, unsigned short port);
#define SEEDBOX_READY_IOPERM readiness_ioperm
#define SEEDBOX_READY_OUTL readiness_outl
#define SEEDBOX_TEST_READY_DELAY_NS 10000000ULL
#define SEEDBOX_SUPERVISOR_TEST
#include "compose-init.c"
#include <assert.h>
#include <sys/socket.h>
static int readiness_permissions, readiness_writes;
static pid_t readiness_child;
static struct timespec readiness_started;
static int readiness_ioperm(unsigned long first, unsigned long count, int enabled) {
    assert(first == SEEDBOX_EFFECT_PORT && count == SEEDBOX_EFFECT_PORT_COUNT && enabled == 1);
    assert(fcntl(management, F_GETFL) & O_NONBLOCK);
    assert(fcntl(management, F_GETFD) & FD_CLOEXEC);
    assert(!isatty(management));
    int status; assert(waitpid(readiness_child, &status, WNOHANG) == 0);
    struct timespec now; assert(!clock_gettime(CLOCK_MONOTONIC, &now));
    long long elapsed = (now.tv_sec - readiness_started.tv_sec)*1000000000LL + now.tv_nsec - readiness_started.tv_nsec;
    assert(elapsed >= (long long)SEEDBOX_TEST_READY_DELAY_NS);
    assert(!readiness_permissions && !readiness_writes);
    readiness_permissions = 1;
    return 0;
}
static void readiness_outl(uint32_t id, unsigned short port) {
    assert(readiness_permissions && !readiness_writes);
    assert(id == SEEDBOX_READY_ID && port == SEEDBOX_EFFECT_PORT);
    readiness_writes = 1;
}
int main(int argc, char **argv) {
    if (argc > 1) { for (;;) pause(); }
    int sockets[2]; assert(!socketpair(AF_UNIX,SOCK_STREAM,0,sockets));
    assert(!fcntl(sockets[0], F_SETFL, O_NONBLOCK));
    assert(!fcntl(sockets[0], F_SETFD, FD_CLOEXEC));
    management = sockets[0];
    environment = (char *[]){"PATH=/bin:/usr/bin",NULL}; user=""; working_dir="/";
    arguments = (char *[]){argv[0],"child",NULL};
    pid_t child = start(); assert(child > 0); pid_t original = child;
    /* Named port discovery works with devtmpfs nodes and no udev symlink.
     * The ordinary POSIX open path uses a temporary device file in this test. */
    char discovery[] = "/tmp/seedbox-port-XXXXXX"; assert(mkdtemp(discovery));
    char classes[PATH_MAX], devices[PATH_MAX], port[PATH_MAX], attribute[PATH_MAX], node[PATH_MAX];
    assert(snprintf(classes, sizeof(classes), "%s/class", discovery) > 0);
    assert(snprintf(devices, sizeof(devices), "%s/dev", discovery) > 0);
    assert(!mkdir(classes, 0700) && !mkdir(devices, 0700));
    assert(snprintf(port, sizeof(port), "%s/class/vport3p7", discovery) > 0);
    assert(!mkdir(port, 0700));
    assert(snprintf(attribute, sizeof(attribute), "%s/class/vport3p7/name", discovery) > 0);
    int name = open(attribute, O_WRONLY|O_CREAT|O_TRUNC, 0600); assert(name >= 0);
    assert(write(name, "unrelated\n", 10) == 10); close(name);
    assert(find_management(classes, devices) == -1);
    name = open(attribute, O_WRONLY|O_TRUNC); assert(name >= 0);
    assert(write(name, "seedbox\n", 8) == 8); close(name);
    assert(snprintf(node, sizeof(node), "%s/dev/vport3p7", discovery) > 0);
    int device = open(node, O_RDWR|O_CREAT, 0600); assert(device >= 0); close(device);
    int discovered = open_management(classes, devices); assert(discovered >= 0);
    assert(fcntl(discovered, F_GETFL) & O_NONBLOCK);
    assert(fcntl(discovered, F_GETFD) & FD_CLOEXEC);
    close(discovered);
    assert(!unlink(node) && !unlink(attribute) && !rmdir(port));
    assert(!rmdir(classes) && !rmdir(devices) && !rmdir(discovery));
    /* Announce readiness only after channel opening, initial exec, and the
     * retained readiness delay. The wrappers check the complete port grant. */
    readiness_child = child;
    environment = (char *[]){"PATH=/bin:/usr/bin", "SEEDBOX_TEST_DELAY_READY=1", NULL};
    assert(!clock_gettime(CLOCK_MONOTONIC, &readiness_started));
    management_ready();
    environment = (char *[]){"PATH=/bin:/usr/bin", NULL};
    assert(readiness_permissions && readiness_writes);
    assert(write(sockets[1], "stop ", 5) == 5);
    relay_management(&child); assert(child == original && command_used == 5);
    assert(write(sockets[1], "1\n", 2) == 2);
    relay_management(&child); assert(child == -1 && !command_used);
    int status; assert(waitpid(original,&status,WNOHANG) == -1 && errno == ECHILD);
    char buffer[512]; ssize_t n = read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0;
    assert(strstr(buffer,"SEEDBOX_EFFECT id=1 action=stop guest_ns="));
    const char *commands = "stop 2\nstart 3\n";
    assert(write(sockets[1], commands, strlen(commands)) == (ssize_t)strlen(commands));
    relay_management(&child); assert(child > 0 && !command_used);
    n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0;
    assert(strstr(buffer,"SEEDBOX_FAILURE id=2") && strstr(buffer,"SEEDBOX_EFFECT id=3 action=start"));
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
    char reply[]="SBX_REPLY1 1 {\"version\":1,\"id\":7,\"ok\":true}";
    assert(write(sockets[1], reply, 8) == 8); relay_management(&child); assert(waiting_relay);
    assert(write(sockets[1], reply + 8, strlen(reply) - 8) == (ssize_t)strlen(reply) - 8);
    assert(write(sockets[1], "\n", 1) == 1); relay_management(&child); assert(!waiting_relay);
    n=read(peer,buffer,sizeof(buffer)-1);assert(n>0);buffer[n]=0;assert(!strcmp(buffer,"{\"version\":1,\"id\":7,\"ok\":true}\n"));
    /* A stale reply must not be delivered to a later call. */
    pid_t check=fork();assert(check>=0);if(!check){control(reply,&child);_exit(0);}
    assert(waitpid(check,&status,0)==check);assert(WIFEXITED(status)&&WEXITSTATUS(status)==125);
    close(peer);relay_requests();assert(control_client==-1);assert(control_connected);
    unlink(address.sun_path);rmdir(directory);
}
