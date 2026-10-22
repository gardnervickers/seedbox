/* Freestanding x86-64 PID 1: hand off live tasks with controlled FP banks. */
typedef unsigned long u64;
typedef unsigned u32;
enum { WORKERS = 5, BYTES = 832 };
static int ready[WORKERS][2], release[WORKERS][2], parked[2];
static volatile unsigned char image[BYTES] __attribute__((aligned(64)));
static const unsigned char markers[WORKERS][10] = {
    {0x31,0x71,0x52,0x93,0xb4,0xd5,0xe6,0x80,0x00,0x40},
    {0x32,0x71,0x52,0x93,0xb4,0xd5,0xe6,0x80,0x00,0x40},
    {0x33,0x71,0x52,0x93,0xb4,0xd5,0xe6,0x80,0x00,0x40},
    {0x34,0x71,0x52,0x93,0xb4,0xd5,0xe6,0x80,0x00,0x40},
    {0x35,0x71,0x52,0x93,0xb4,0xd5,0xe6,0x80,0x00,0x40},
};
/* The compiler must not use SIMD between a restore and the blocking syscall. */
#define INTEGER __attribute__((target("no-sse,no-sse2,no-mmx,no-avx")))
static INTEGER long syscall3(long number, long a, long b, long c) {
    long result;
    __asm__ volatile("syscall" : "=a"(result)
        : "a"(number), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return result;
}
static INTEGER void out(const char *text) {
    volatile long length = 0;
    while (text[length]) ++length;
    syscall3(1, 1, (long)text, length);
}
static INTEGER __attribute__((noreturn)) void fail(const char *message) {
    out(message);
    __builtin_trap();
}
static INTEGER void token(long operation, int fd) {
    char value = 1;
    if (syscall3(operation, fd, (long)&value, 1) != 1)
        fail("XSAVE_TASK_ERROR: pipe token\n");
}
static INTEGER void restore(u64 present, u32 mxcsr, int xmm, int ymm) {
    for (unsigned i = 0; i < BYTES; ++i) image[i] = 0;
    *(u32 *)(image + 24) = mxcsr;
    *(u64 *)(image + 512) = present;
    for (unsigned i = 0; i < 256; ++i) {
        image[160 + i] = xmm ? (i ^ 0xa5) : 0;
        image[576 + i] = ymm ? (i ^ 0x3c) : 0;
    }
    /* User XRSTOR takes a standard-format image. Its MXCSR semantics differ
     * from the compacted banks produced by the kernel's XSAVES task saves. */
    __asm__ volatile("xrstor (%0)" : : "r"(image), "a"(6), "d"(0) : "memory");
}
static INTEGER __attribute__((noreturn)) void worker(unsigned number) {
    __asm__ volatile("fninit; fldt (%0)" : : "r"(markers[number]) : "memory");
    if (number == 0) {
        restore(2, 0x1fa0, 1, 0);
        token(1, ready[number][1]);
        /* The controller's reply proves a save of this noninitial SSE bank. */
        token(0, release[number][0]);
        restore(0, 0x1f80, 0, 0);
    } else if (number == 1) {
        restore(2, 0x1f80, 0, 0);
    } else if (number == 2) {
        restore(6, 0x5f80, 1, 1);
    } else if (number == 3) {
        /* Zero XMM with a live precision flag is not initial SSE state. */
        restore(2, 0x1fa0, 0, 0);
    } else {
        /* Active YMM must not make an absent SSE component own stale bytes. */
        restore(4, 0x1f80, 0, 1);
    }
    token(1, ready[number][1]);
    token(0, parked[0]);
    fail("XSAVE_TASK_ERROR: worker parking pipe unexpectedly released\n");
}
__asm__(".globl _start\n_start:\n\tand $-16, %rsp\n\tcall start\n\thlt");
INTEGER __attribute__((noreturn)) void start(void) {
    u32 eax = 1, ebx, ecx, edx;
    __asm__ volatile("cpuid" : "+a"(eax), "=b"(ebx), "=c"(ecx), "=d"(edx));
    if ((ecx & (7U << 26)) != (7U << 26))
        fail("XSAVE_TASK_ERROR: requires XSAVE, OSXSAVE and AVX\n");
    __asm__ volatile("xgetbv" : "=a"(eax), "=d"(edx) : "c"(0));
    if ((eax & 7) != 7)
        fail("XSAVE_TASK_ERROR: requires enabled x87/SSE/AVX\n");
    eax = 0xd; ecx = 1;
    __asm__ volatile("cpuid" : "+a"(eax), "=b"(ebx), "+c"(ecx), "=d"(edx));
    if (!(eax & (2 | 8)))
        fail("XSAVE_TASK_ERROR: requires XSAVEC or XSAVES task-save support\n");
    if (syscall3(293, (long)parked, 0, 0)) fail("XSAVE_TASK_ERROR: pipe2\n");
    for (unsigned i = 0; i < WORKERS; ++i) {
        if (syscall3(293, (long)ready[i], 0, 0) ||
            syscall3(293, (long)release[i], 0, 0))
            fail("XSAVE_TASK_ERROR: pipe2\n");
        long child = syscall3(57, 0, 0, 0);
        if (child < 0) fail("XSAVE_TASK_ERROR: fork\n");
        if (!child) worker(i);
        token(0, ready[i][0]);
        if (i == 0) {
            token(1, release[i][1]);
            token(0, ready[i][0]);
        }
        /* Receiving each final token requires handing the single guest CPU
         * back to this controller, saving that worker's final FP bank. */
    }
    out("XSAVE_TASK_BANKS_READY\n");
    token(0, parked[0]);
    fail("XSAVE_TASK_ERROR: controller parking pipe unexpectedly released\n");
}
