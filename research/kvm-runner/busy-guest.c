#include <sys/io.h>
#include <sys/mount.h>
#include <unistd.h>
/* Primary preemption fixture: no syscall/PIO during the counted loop.
 * Initialization/exit are explicit natural boundaries. Static fixed-address ELF.
 */
int main(void) {
 if (iopl(3)) return 1;
 outb(1,0x530);
 unsigned long value;
 asm volatile(
  "xor %%rax,%%rax\n"
  "mov $100000000,%%rcx\n"
  ".global seedbox_busy_start\nseedbox_busy_start:\n"
  "inc %%rax\ndec %%rcx\njnz seedbox_busy_start\n"
  ".global seedbox_busy_end\nseedbox_busy_end:\n"
  : "=a"(value) : : "rcx","cc","memory");
 outb(2,0x530);
 outl(value==100000000 ? 16:17,0xf4);
 for (;;) {}
}
