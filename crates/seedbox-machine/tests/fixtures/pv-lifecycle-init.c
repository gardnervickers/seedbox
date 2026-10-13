/* Freestanding ARM64 Linux PID 1. It owns the guest's shutdown policy: a
 * lifecycle request sends SIGTERM to its child, waits for cleanup evidence,
 * syncs, then asks Linux to power off. No host application-control protocol. */
typedef unsigned long u64;
static long call(long number,long a,long b,long c,long d,long e,long f) {
    register long x8 __asm__("x8")=number;
    register long x0 __asm__("x0")=a,x1 __asm__("x1")=b,x2 __asm__("x2")=c;
    register long x3 __asm__("x3")=d,x4 __asm__("x4")=e,x5 __asm__("x5")=f;
    __asm__ volatile("svc 0":"+r"(x0):"r"(x8),"r"(x1),"r"(x2),"r"(x3),"r"(x4),"r"(x5):"memory");return x0;
}
static unsigned length(const char *s){unsigned n=0;while(s[n])n++;return n;}
static void say(const char *s){call(64,1,(long)s,length(s),0,0,0);}
__attribute__((noreturn)) static void wait_forever(void){for(;;)call(73,0,0,0,0,8,0);}
__attribute__((noreturn)) static void fail(void){say("PV_SHUTDOWN_FAILED\n");wait_forever();}
static int open(const char *path,int flags,int mode){return call(56,-100,(long)path,flags,mode,0,0);}
static void cleanup(int signal){(void)signal;int fd=open("/child-cleaned",0x41,0600);if(fd<0)fail();if(call(64,fd,(long)"done",4,0,0,0)!=4)fail();call(57,fd,0,0,0,0,0);say("PV_CHILD_CLEANED\n");call(93,0,0,0,0,0,0);wait_forever();}
struct action {void (*handler)(int);u64 flags;void (*restorer)(void);u64 mask;};
__attribute__((noreturn)) void _start(void) {
    if(call(40,(long)"devtmpfs",(long)"/dev",(long)"devtmpfs",0,0,0)<0)fail();
    if(call(40,(long)"sysfs",(long)"/sys",(long)"sysfs",0,0,0)<0)fail();
    int fd=-1;
    for(unsigned attempt=0;attempt<10000;attempt++){
        fd=open("/dev/vport0p0",2,0);if(fd>=0)break;
        long delay[2]={0,1000000};call(101,(long)delay,0,0,0,0,0);
    }
    if(fd<0)fail();
    int name=open("/sys/class/virtio-ports/vport0p0/name",0,0);char port[32]={0};
    if(name<0||call(63,name,(long)port,31,0,0,0)<16)fail();
    const char *expected="seedbox-lifecycle";for(unsigned i=0;i<length(expected);i++)if(port[i]!=expected[i])fail();
    call(57,name,0,0,0,0,0);
    /* Install before clone so even an early host request cannot race child
     * handler registration. Both processes inherit it; only child is signaled. */
    struct action action={cleanup,0,0,0};if(call(134,15,(long)&action,0,8,0,0)<0)fail();
    int child=call(220,17,0,0,0,0,0);if(child<0)fail();if(child==0)wait_forever();
    say("PV_LIFECYCLE_READY\n");
    char command[9];unsigned used=0;
    while(used<sizeof(command)){long count=call(63,fd,(long)(command+used),sizeof(command)-used,0,0,0);if(count<=0)fail();used+=(unsigned)count;}
    const char *request="shutdown\n";for(unsigned i=0;i<9;i++)if(command[i]!=request[i])fail();
    say("PV_SHUTDOWN_REQUESTED\n");if(call(129,child,15,0,0,0,0)<0)fail();
    int status=-1;if(call(260,child,(long)&status,0,0,0,0)!=child||status!=0)fail();
    int marker=open("/child-cleaned",0,0);char done[4];if(marker<0||call(63,marker,(long)done,4,0,0,0)!=4)fail();
    if(done[0]!='d'||done[1]!='o'||done[2]!='n'||done[3]!='e')fail();
    say("PV_SHUTDOWN_COMPLETE\n");call(81,0,0,0,0,0,0);
    call(142,0xfee1dead,0x28121969,0x4321fedc,0,0,0);fail();
}
