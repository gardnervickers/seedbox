/* Freestanding PID 1 for the PV stop-invariance test (rule C3). Two processes
 * share the one guest CPU; each charges syscalls, reads the clock and sleeps,
 * and folds every clock value it reads into a hash it prints. Where the host
 * ends the machine's runs must change none of it. */
typedef unsigned long u64;
#include "pv-syscall.h"
static void say(const char *s, unsigned n) { call(64, 1, (long)s, n, 0, 0, 0); }
static u64 now(void) {
    long t[2] = {0, 0};
    call(113, 1, (long)t, 0, 0, 0, 0);
    return (u64)t[0] * 1000000000UL + (u64)t[1];
}
static void nap(u64 ns) {
    long t[2] = {0, (long)ns};
    call(101, (long)t, 0, 0, 0, 0, 0);
}
static u64 mix(u64 h, u64 v) { return (h ^ v) * 0x100000001b3UL; }
static void line(const char *tag, u64 v) {
    char out[48];
    unsigned n = 0;
    while (tag[n]) {
        out[n] = tag[n];
        n++;
    }
    out[n++] = ' ';
    for (int shift = 60; shift >= 0; shift -= 4)
        out[n++] = "0123456789abcdef"[(v >> shift) & 15];
    out[n++] = '\n';
    say(out, n);
}
__attribute__((noreturn)) void _start(void) {
    long child = call(220, 17, 0, 0, 0, 0, 0);
    u64 h = child == 0 ? 0xc0ffee : 0xcbf29ce484222325UL;
    for (unsigned round = 0; round < 600; round++) {
        unsigned spin = child == 0 ? 37 + round % 11 : 50 + (round * 7) % 23;
        /* Charged syscalls: the guest kernel's bounded clock reads. */
        for (unsigned j = 0; j < spin; j++)
            h = mix(h, (u64)call(172, 0, 0, 0, 0, 0, 0) + j);
        h = mix(h, now());
        nap(child == 0 ? 7000 + (round % 5) * 1000 : round % 3 == 0 ? 0 : 11000 + (round % 4) * 1000);
        h = mix(h, now());
        if (round % 100 == 99)
            line(child == 0 ? "C3_CHILD" : "C3_PARENT", h);
    }
    line(child == 0 ? "C3_CHILD_DONE" : "C3_PARENT_DONE", h);
    for (;;)
        call(73, 0, 0, 0, 0, 8, 0);
}
