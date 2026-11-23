/* Freestanding x86-64 PID 1: deliver signals over controlled FP banks and
 * check the XSAVE image each signal frame holds. */
typedef unsigned long u64;
typedef unsigned u32;
enum { BYTES = 832, STACK = 65536 };
static volatile unsigned char image[BYTES] __attribute__((aligned(64)));
static volatile unsigned char frame[BYTES];
static volatile unsigned char after[BYTES] __attribute__((aligned(64)));
static unsigned char altstack[STACK] __attribute__((aligned(64)));
static volatile int delivered;
/* The compiler must not use SIMD between a restore and the signal frame. */
#define INTEGER __attribute__((target("no-sse,no-sse2,no-mmx,no-avx")))
static INTEGER long syscall4(long number, long a, long b, long c, long d) {
    long result;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(result)
        : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return result;
}
static INTEGER void out(const char *text) {
    volatile long length = 0;
    while (text[length]) ++length;
    syscall4(1, 1, (long)text, length, 0);
}
static INTEGER __attribute__((noreturn)) void fail(const char *message) {
    out(message);
    __builtin_trap();
}
static INTEGER u64 word(const volatile unsigned char *bytes) {
    u64 value = 0;
    for (unsigned i = 0; i < 8; ++i) value |= (u64)bytes[i] << (8 * i);
    return value;
}
static INTEGER int zero(const volatile unsigned char *bytes, unsigned from, unsigned to) {
    for (unsigned i = from; i < to; ++i)
        if (bytes[i]) return 0;
    return 1;
}
static INTEGER int pattern(const volatile unsigned char *bytes, unsigned from, unsigned char key) {
    for (unsigned i = 0; i < 256; ++i)
        if (bytes[from + i] != (unsigned char)(i ^ key)) return 0;
    return 1;
}
/* The kernel's rt_sigframe: ucontext follows the return address, and
 * uc_mcontext.fpstate points at the standard-format XSAVE image. */
static INTEGER void handler(int signal, void *info, void *context) {
    (void)signal;
    (void)info;
    const volatile unsigned char *fpstate = *(unsigned char *const *)((char *)context + 224);
    for (unsigned i = 0; i < BYTES; ++i) frame[i] = fpstate[i];
    delivered = 1;
}
__asm__(".globl restorer\nrestorer:\n\tmov $15, %eax\n\tsyscall\n\thlt");
void restorer(void);
/* Restore initial x87, a 0x5a XMM marker or zero XMM, and a 0x3c YMM marker
 * or zero YMM, with `present` as XSTATE_BV; then take a signal on an
 * alternate stack. */
static INTEGER void deliver(u64 present, int xmm, int ymm) {
    for (unsigned i = 0; i < BYTES; ++i) image[i] = 0;
    image[0] = 0x7f;
    image[1] = 0x03;
    image[24] = 0x80; /* MXCSR 0x1f80 */
    image[25] = 0x1f;
    for (unsigned i = 0; i < 8; ++i) image[512 + i] = (unsigned char)(present >> (8 * i));
    for (unsigned i = 0; i < 256; ++i) {
        image[160 + i] = xmm ? (unsigned char)(i ^ 0x5a) : 0;
        image[576 + i] = ymm ? (unsigned char)(i ^ 0x3c) : 0;
    }
    delivered = 0;
    __asm__ volatile("xrstor (%0)" : : "r"(image), "a"(7), "d"(0) : "memory");
    /* kill(1, SIGUSR1) to this PID 1: delivered as the syscall returns. */
    if (syscall4(62, 1, 10, 0, 0)) fail("SIGFRAME_ERROR: kill\n");
    __asm__ volatile("xsave (%0)" : : "r"(after), "a"(7), "d"(0) : "memory");
    if (!delivered) fail("SIGFRAME_ERROR: signal not delivered\n");
}
static INTEGER void expect(int ok, const char *message) {
    if (!ok) fail(message);
}
/* x87 and SSE are always present in a frame; PKRU is the kernel's own. Every
 * other component is present exactly when it holds noninitial (nonzero) state;
 * nothing here touches AVX-512 or AMX tile configuration. */
static INTEGER void bits(u64 ymm, const char *message) {
    u64 present = word(frame + 512);
    if ((present & 3) == 3 && (present & 4) == ymm && !(present & ~(u64)0x207))
        return;
    char hex[] = "XSTATE_BV=0x0000000000000000\n";
    for (unsigned i = 0; i < 16; ++i)
        hex[12 + i] = "0123456789abcdef"[(present >> (60 - 4 * i)) & 15];
    out(hex);
    fail(message);
}
__asm__(".globl _start\n_start:\n\tand $-16, %rsp\n\tcall start\n\thlt");
INTEGER __attribute__((noreturn)) void start(void) {
    u32 eax = 1, ebx, ecx, edx;
    __asm__ volatile("cpuid" : "+a"(eax), "=b"(ebx), "=c"(ecx), "=d"(edx));
    if ((ecx & (7U << 26)) != (7U << 26))
        fail("SIGFRAME_ERROR: requires XSAVE, OSXSAVE and AVX\n");
    __asm__ volatile("xgetbv" : "=a"(eax), "=d"(edx) : "c"(0));
    if ((eax & 7) != 7)
        fail("SIGFRAME_ERROR: requires enabled x87/SSE/AVX\n");
    long stack[3] = {(long)altstack, 0, STACK};
    if (syscall4(131, (long)stack, 0, 0, 0)) fail("SIGFRAME_ERROR: sigaltstack\n");
    /* handler, SA_SIGINFO | SA_ONSTACK | SA_RESTORER, restorer, empty mask */
    long action[4] = {(long)handler, 0x4 | 0x08000000 | 0x04000000, (long)restorer, 0};
    if (syscall4(13, 10, (long)action, 0, 8)) fail("SIGFRAME_ERROR: rt_sigaction\n");

    /* Zero YMM loaded as present is what a host FPU save/restore can leave. */
    deliver(7, 1, 0);
    bits(0, "SIGFRAME_ERROR: zero YMM recorded present\n");
    expect(pattern(frame, 160, 0x5a), "SIGFRAME_ERROR: live XMM changed\n");
    expect(zero(frame, 576, 832), "SIGFRAME_ERROR: zero YMM bytes\n");
    expect(pattern(after, 160, 0x5a), "SIGFRAME_ERROR: XMM after sigreturn\n");
    expect(zero(after, 576, 832), "SIGFRAME_ERROR: YMM after sigreturn\n");

    deliver(0, 0, 0);
    bits(0, "SIGFRAME_ERROR: absent YMM recorded present\n");

    /* Live YMM stays present and intact through the frame and sigreturn. */
    deliver(6, 0, 1);
    bits(4, "SIGFRAME_ERROR: live YMM recorded absent\n");
    expect(pattern(frame, 576, 0x3c), "SIGFRAME_ERROR: live YMM changed\n");
    expect(pattern(after, 576, 0x3c), "SIGFRAME_ERROR: live YMM after sigreturn\n");

    out("SIGFRAME_CANONICAL\n");
    for (;;) syscall4(34, 0, 0, 0, 0); /* pause() */
}
