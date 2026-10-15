/* _Unwind_Backtrace from a SIGSEGV handler: native walks handler -> vDSO
 * __kernel_rt_sigreturn -> faulting frames -> main. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>
#include <unwind.h>
static sigjmp_buf jb;
static _Unwind_Reason_Code note(struct _Unwind_Context *c, void *p) {
    int *n = p;
    uintptr_t ip = _Unwind_GetIP(c);
    Dl_info d = {0};
    dladdr((void *)ip, &d);
    printf("frame %d ip-vdso %+ld %s\n", (*n)++, (long)(ip - getauxval(AT_SYSINFO_EHDR)), d.dli_sname ? d.dli_sname : "?");
    return _URC_NO_REASON;
}
__attribute__((noipa)) static void handler(int s) {
    (void)s;
    int n = 0;
    printf("lr-vdso %+ld\n", (long)((uintptr_t)__builtin_return_address(0) - getauxval(AT_SYSINFO_EHDR)));
    uint32_t *lr = __builtin_return_address(0);
    printf("lr words %08x %08x\n", lr[0], lr[1]);
    char line[512]; FILE *m = fopen("/proc/self/maps", "r");
    while (fgets(line, sizeof line, m)) { unsigned long a, b; if (sscanf(line, "%lx-%lx", &a, &b) == 2 && (uintptr_t)lr >= a && (uintptr_t)lr < b) printf("lr map %s", line); }
    fclose(m);
    int r = _Unwind_Backtrace(note, &n);
    printf("frames %d reason %d\n", n, r);
    siglongjmp(jb, 1);
}
__attribute__((noipa)) void faulting(volatile int *p) { *p = 1; }
int main(void) {
    signal(SIGSEGV, handler);
    if (!sigsetjmp(jb, 1)) faulting((volatile int *)8);
    return 0;
}
