#define _POSIX_C_SOURCE 200809L
#include <arpa/inet.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>
static uint64_t now(void) { struct timespec t; if(clock_gettime(CLOCK_MONOTONIC,&t)) exit(3); return (uint64_t)t.tv_sec*1000000000+t.tv_nsec; }
static struct sockaddr_in addr(const char *ip, int port) { struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(port)}; if(inet_pton(AF_INET,ip,&a.sin_addr)!=1) exit(3); return a; }
int main(int argc,char **argv) {
 if(argc<2)return 3;
 int fd=socket(AF_INET,SOCK_DGRAM,0); if(fd<0){perror("socket");return 3;}
 if(!strcmp(argv[1],"serve")) {
  struct sockaddr_in a=addr("0.0.0.0",9090); if(bind(fd,(void*)&a,sizeof(a))){perror("bind");return 3;}
  for(;;){ char b[64]; socklen_t n=sizeof(a); int r=recvfrom(fd,b,sizeof(b),0,(void*)&a,&n); if(r>0)sendto(fd,b,r,0,(void*)&a,n); }
 }
 if(argc<3)return 3;
 struct sockaddr_in a=addr(argv[2],9090);
 if(!strcmp(argv[1],"bind-deny")) {
  int r=bind(fd,(void*)&a,sizeof(a)); int e=errno;
  printf("ISOLATION bind=%s rc=%d errno=%d\n",argv[2],r,e);
  return r==-1 && e==EADDRNOTAVAIL ? 0:1;
 }
 if(argc<5)return 3;
 int blocked=!strcmp(argv[3],"blocked"); uint64_t floor=strtoull(argv[4],0,10);
 struct timeval timeout={.tv_sec=0,.tv_usec=30000}; if(setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout))){perror("setsockopt");return 3;}
 const char msg[]="seedbox-network-probe"; char reply[64]; uint64_t t=now();
 if(sendto(fd,msg,sizeof(msg),0,(void*)&a,sizeof(a))<0){perror("sendto");return 3;}
 int r=recvfrom(fd,reply,sizeof(reply),0,0,0); int e=r<0?errno:0; uint64_t elapsed=now()-t;
 int good=blocked ? (r<0 && (e==EAGAIN || e==EWOULDBLOCK)) : (r==sizeof(msg) && !memcmp(msg,reply,sizeof(msg)) && elapsed>=floor);
 printf("PROBE target=%s expected=%s recv=%d errno=%d rtt_ns=%llu floor_ns=%llu pass=%d\n",argv[2],argv[3],r,e,(unsigned long long)elapsed,(unsigned long long)floor,good);
 return good?0:1;
}
