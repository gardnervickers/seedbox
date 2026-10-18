/* Freestanding Linux syscalls for native PV fixtures, by the generic (arm64)
 * numbers the fixtures use. x86-64 maps each to its own syscall number. */
#if defined(__aarch64__)
#define PV_SIGNAL_FLAGS 0UL
static long call(long number, long a, long b, long c, long d, long e, long f) {
    register long x8 __asm__("x8") = number;
    register long x0 __asm__("x0") = a, x1 __asm__("x1") = b, x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d, x4 __asm__("x4") = e, x5 __asm__("x5") = f;
    __asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5)
                     : "memory");
    return x0;
}
#elif defined(__x86_64__)
/* x86-64 delivers handlers only with SA_RESTORER and a restorer address. */
#define PV_SIGNAL_FLAGS 0x04000000UL
static long call(long number, long a, long b, long c, long d, long e, long f) {
    long native;
    switch (number) {
    case 40: native = 165; break;  /* mount */
    case 56: native = 257; break;  /* openat */
    case 57: native = 3; break;    /* close */
    case 63: native = 0; break;    /* read */
    case 64: native = 1; break;    /* write */
    case 73: native = 271; break;  /* ppoll */
    case 81: native = 162; break;  /* sync */
    case 93: native = 60; break;   /* exit */
    case 101: native = 35; break;  /* nanosleep */
    case 113: native = 228; break; /* clock_gettime */
    case 129: native = 62; break;  /* kill */
    case 134: native = 13; break;  /* rt_sigaction */
    case 142: native = 169; break; /* reboot */
    case 172: native = 39; break;  /* getpid */
    case 220: native = 56; break;  /* clone */
    case 260: native = 61; break;  /* wait4 */
    default: for (;;) __builtin_trap();
    }
    register long r10 __asm__("r10") = d, r8 __asm__("r8") = e, r9 __asm__("r9") = f;
    long result;
    __asm__ volatile("syscall" : "=a"(result)
                     : "a"(native), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9)
                     : "rcx", "r11", "memory");
    return result;
}
#else
#error unsupported PV fixture architecture
#endif
