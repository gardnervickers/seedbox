/* Freestanding x86-64 application for instrumented PV instruction checks.
 *
 *   entropy  execute RDRAND and RDSEED at 16, 32 and 64 bits regardless of
 *            CPUID, with a sentinel destination and every arithmetic flag set
 *            beforehand; print the destination and arithmetic flags after.
 *   probe    run each host-sensitive user instruction in a child process and
 *            print its result or the signal that ended the child.
 */
typedef unsigned long u64;

static long sys(long number, long a, long b, long c, long d) {
    register long r10 __asm__("r10") = d;
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10)
                     : "rcx", "r11", "memory");
    return result;
}
static void out(const char *text) {
    /* volatile: keep the compiler from emitting a strlen call. */
    volatile long n = 0;
    while (text[n]) ++n;
    sys(1, 1, (long)text, n, 0);
}
static void hex(u64 value) {
    char text[18];
    for (int i = 15; i >= 0; --i, value >>= 4) text[i] = "0123456789abcdef"[value & 15];
    text[16] = ' ';
    text[17] = 0;
    out(text);
}

#define ARITHMETIC 0x8d5UL /* OF SF ZF AF PF CF */
/* Destination sentinel and RFLAGS with every arithmetic flag except CF set,
 * so the instruction must set CF and clear the others itself. */
#define ENTROPY(name, insn, reg)                                                     \
    static void name(void) {                                                         \
        u64 value = 0xa5a5a5a5a5a5a5a5UL, flags = 0x8d4UL | 2;                       \
        __asm__ volatile("push %1\n\tpopf\n\t" insn "\n\tpushf\n\tpop %1"              \
                         : "+" reg(value), "+r"(flags) : : "cc");                    \
        out(#name " ");                                                              \
        hex(value);                                                                  \
        hex(flags & ARITHMETIC);                                                     \
        out("\n");                                                                   \
    }
ENTROPY(rdrand16, "rdrand %w0", "r")
ENTROPY(rdrand32, "rdrand %k0", "r")
ENTROPY(rdrand64, "rdrand %q0", "r")
ENTROPY(rdseed16, "rdseed %w0", "r")
ENTROPY(rdseed32, "rdseed %k0", "r")
ENTROPY(rdseed64, "rdseed %q0", "r")

static void probe(const char *name, void (*run)(void)) {
    out(name);
    out(" ");
    long child = sys(57, 0, 0, 0, 0); /* fork */
    if (child == 0) {
        run();
        sys(60, 0, 0, 0, 0);
    }
    int status = 0;
    sys(61, child, (long)&status, 0, 0); /* wait4 */
    if (status & 0x7f) {
        out("signal ");
        hex((u64)(status & 0x7f));
    }
    out("\n");
}
static void rdpid(void) {
    u64 value;
    __asm__ volatile("rdpid %0" : "=r"(value));
    hex(value);
}
static void rdpru(void) {
    unsigned lo, hi;
    __asm__ volatile(".byte 0x0f, 0x01, 0xfd" : "=a"(lo), "=d"(hi) : "c"(1));
    hex((u64)hi << 32 | lo);
}
static void rdpmc(void) {
    unsigned lo, hi;
    __asm__ volatile("rdpmc" : "=a"(lo), "=d"(hi) : "c"(0));
    hex((u64)hi << 32 | lo);
}
static void rdtscp(void) {
    unsigned lo, hi, aux;
    __asm__ volatile("rdtscp" : "=a"(lo), "=d"(hi), "=c"(aux));
    /* Virtual time only: report that it is nonzero, and TSC_AUX. */
    hex((u64)hi << 32 | lo ? 1 : 0);
    hex(aux);
}
static u64 line;
static void monitorx(void) {
    /* MWAITX with a timer would wait on host time if it ran natively. */
    __asm__ volatile(".byte 0x0f, 0x01, 0xfa" : : "a"(&line), "c"(0), "d"(0));
    __asm__ volatile(".byte 0x0f, 0x01, 0xfb" : : "a"(0), "b"(1000000), "c"(2));
    hex(1);
}
static void umwait(void) {
    __asm__ volatile(".byte 0xf2, 0x0f, 0xae, 0xf1" : : "a"(0), "d"(0), "c"(0));
    hex(1);
}
__asm__(".globl _start\n_start:\n\tmov %rsp, %rdi\n\tand $-16, %rsp\n\tcall start\n\thlt");
void start(u64 *stack) {
    const char *mode = stack[0] > 1 ? (const char *)stack[2] : "";
    if (mode[0] == 'e') {
        rdrand16();
        rdrand32();
        rdrand64();
        rdrand64();
        rdseed16();
        rdseed32();
        rdseed64();
        rdseed64();
    } else if (mode[0] == 'p') {
        probe("rdpid", rdpid);
        probe("rdpru", rdpru);
        probe("rdpmc", rdpmc);
        probe("rdtscp", rdtscp);
        probe("monitorx", monitorx);
        probe("umwait", umwait);
    } else {
        out("usage: entropy | probe\n");
        sys(60, 2, 0, 0, 0);
    }
    sys(60, 0, 0, 0, 0);
}
