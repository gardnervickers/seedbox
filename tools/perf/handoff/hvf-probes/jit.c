#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <string.h>
int main(void) {
 uint64_t ctr; __asm__ volatile("mrs %0, ctr_el0":"=r"(ctr));
 printf("CTR_EL0=%#lx DIC=%lu IDC=%lu\n",ctr,(ctr>>29)&1,(ctr>>28)&1);
 uint32_t *code=mmap(0,4096,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
 if(code==MAP_FAILED) return 2;
 for(unsigned i=0;i<100;i++) {
  code[0]=0x52800000 | (i<<5); // mov w0,#i
  code[1]=0xd65f03c0; // ret
  __builtin___clear_cache((char*)code,(char*)code+8);
  int got=((int(*)(void))code)();
  if(got!=(int)i) {printf("MISMATCH i=%u got=%d\n",i,got); return 1;}
 }
 puts("100 code rewrites: PASS"); return 0;
}
