#define _GNU_SOURCE
#include <sys/mount.h>
#include <sys/wait.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/io.h>
#include <sys/ioctl.h>
#include <net/if.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <time.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
static void msg(const char*s){while(*s)outb(*s++,0xe9);}
static void finish(int ok){msg(ok?"W2 PASS\n":"W2 FAIL\n");outl(ok?16:17,0xf4);for(;;);}
int main(){iopl(3);mount("proc","/proc","proc",0,0);mount("devtmpfs","/dev","devtmpfs",0,0);syscall(9999);
 int net=socket(AF_INET,SOCK_DGRAM,0);struct ifreq req={0};strcpy(req.ifr_name,"lo");req.ifr_flags=IFF_UP|IFF_LOOPBACK|IFF_RUNNING;if(ioctl(net,SIOCSIFFLAGS,&req)){msg("loopback setup failed\n");finish(0);}close(net);unsigned long entropy=0;int rf=open("/dev/urandom",O_RDONLY);int nr=rf>=0?read(rf,&entropy,8):-1;if(rf>=0)close(rf);char sample[128];snprintf(sample,sizeof(sample),"guest_urandom bytes=%d value=%016lx\n",nr,entropy);msg(sample);pid_t p=fork();if(!p){int f=open("/dev/null",O_RDWR);dup2(f,0);dup2(f,1);dup2(f,2);execl("/redis-server","redis-server","--save","","--appendonly","no","--protected-mode","no",NULL);_exit(5);}
 int fd=-1;struct sockaddr_in addr={.sin_family=AF_INET,.sin_port=htons(6379),.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
 for(int t=0;t<100;t++){fd=socket(AF_INET,SOCK_STREAM,0);if(connect(fd,(void*)&addr,sizeof(addr))==0)break;close(fd);fd=-1;struct timespec ts={.tv_nsec=10000000};nanosleep(&ts,0);}if(fd<0){msg("redis connect failed\n");finish(0);}
 char b[128];for(int n=0;n<1000;n++){const char*s="*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";if(write(fd,s,strlen(s))!=(int)strlen(s)||read(fd,b,5)!=5||memcmp(b,"+OK\r\n",5))finish(0);s="*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";if(write(fd,s,strlen(s))!=(int)strlen(s)||read(fd,b,7)!=7||memcmp(b,"$1\r\nv\r\n",7))finish(0);}
 msg("redis_loopback operations=2000 values_checked=1000\n");
 outb(1,0x528);struct timespec a,z;clock_gettime(CLOCK_MONOTONIC,&a);unsigned polls=0;do{clock_gettime(CLOCK_MONOTONIC,&z);polls++;}while((z.tv_sec-a.tv_sec)*1000000000L+z.tv_nsec-a.tv_nsec<1000000L&&polls<100000);
 snprintf(b,sizeof(b),"clock_poll count=%u elapsed_ns=%ld\n",polls,(z.tv_sec-a.tv_sec)*1000000000L+z.tv_nsec-a.tv_nsec);msg(b);finish(polls<100000);
}
