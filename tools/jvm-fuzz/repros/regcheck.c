/*
 * Register integrity across translator instrumentation (AArch64). Loads a known
 * pattern into x0-x17, x19-x26 and the full 128 bits of q0-q31 (x27 carries the
 * argument block), runs a loop that
 * touches only x28, x29 and the flags, then stores everything and compares. With
 * the Seedbox progress client and a small instruction budget the loop crosses
 * many checkpoints, each of which redirects execution through a full machine
 * context. Natively it prints "OK".
 *
 * Usage: regcheck ROUNDS [SPINS [x28]]
 * With "x28" the loop keeps its counter in memory addressed through x28, like
 * HotSpot's thread register, so the translator mangles every loop access.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define NX 26 /* x0-x17 and x19-x26 */
#define NQ 32

static uint64_t expect_x[NX], seen_x[NX];
static uint64_t expect_q[NQ * 2], seen_q[NQ * 2];

static uint64_t area[2];
static struct {
    uint64_t *ex, *sx, *eq, *sq;
    uint64_t spins;
    uint64_t *area;
    uint64_t x28_mode;
} args = {expect_x, seen_x, expect_q, seen_q, 0, area, 0};

static void run(uint64_t spins) {
    args.spins = spins;
    register void *block __asm__("x27") = &args;
    __asm__ volatile(
        "stp x29, x30, [sp, #-16]!\n"
        /* q0-q31 */
        "ldr x29, [x27, #16]\n"
        "ldp q0, q1, [x29, #0]\n" "ldp q2, q3, [x29, #32]\n" "ldp q4, q5, [x29, #64]\n"
        "ldp q6, q7, [x29, #96]\n" "ldp q8, q9, [x29, #128]\n" "ldp q10, q11, [x29, #160]\n"
        "ldp q12, q13, [x29, #192]\n" "ldp q14, q15, [x29, #224]\n" "ldp q16, q17, [x29, #256]\n"
        "ldp q18, q19, [x29, #288]\n" "ldp q20, q21, [x29, #320]\n" "ldp q22, q23, [x29, #352]\n"
        "ldp q24, q25, [x29, #384]\n" "ldp q26, q27, [x29, #416]\n" "ldp q28, q29, [x29, #448]\n"
        "ldp q30, q31, [x29, #480]\n"
        "ldr x28, [x27, #32]\n"
        "ldr x29, [x27, #48]\n"
        "cbz x29, 2f\n"
        "ldr x29, [x27, #40]\n"
        "str x28, [x29]\n"
        "mov x28, x29\n"
        "2:\n"
        "ldr x29, [x27, #0]\n"
        "ldp x0, x1, [x29, #0]\n" "ldp x2, x3, [x29, #16]\n" "ldp x4, x5, [x29, #32]\n"
        "ldp x6, x7, [x29, #48]\n" "ldp x8, x9, [x29, #64]\n" "ldp x10, x11, [x29, #80]\n"
        "ldp x12, x13, [x29, #96]\n" "ldp x14, x15, [x29, #112]\n" "ldp x16, x17, [x29, #128]\n"
        "ldp x19, x20, [x29, #144]\n" "ldp x21, x22, [x29, #160]\n" "ldp x23, x24, [x29, #176]\n"
        "ldp x25, x26, [x29, #192]\n"
        "ldr x29, [x27, #48]\n"
        "cbnz x29, 3f\n"
        "1:\n"
        "subs x28, x28, #1\n"
        "b.ne 1b\n"
        "b 4f\n"
        "3:\n"
        "ldr x29, [x28, #0]\n"
        "subs x29, x29, #1\n"
        "str x29, [x28, #0]\n"
        "ldr x29, [x28, #8]\n"
        "add x29, x29, #3\n"
        "str x29, [x28, #8]\n"
        "ldr x29, [x28, #0]\n"
        "cbnz x29, 3b\n"
        "4:\n"
        "ldr x29, [x27, #8]\n"
        "stp x0, x1, [x29, #0]\n" "stp x2, x3, [x29, #16]\n" "stp x4, x5, [x29, #32]\n"
        "stp x6, x7, [x29, #48]\n" "stp x8, x9, [x29, #64]\n" "stp x10, x11, [x29, #80]\n"
        "stp x12, x13, [x29, #96]\n" "stp x14, x15, [x29, #112]\n" "stp x16, x17, [x29, #128]\n"
        "stp x19, x20, [x29, #144]\n" "stp x21, x22, [x29, #160]\n" "stp x23, x24, [x29, #176]\n"
        "stp x25, x26, [x29, #192]\n"
        "ldr x29, [x27, #24]\n"
        "stp q0, q1, [x29, #0]\n" "stp q2, q3, [x29, #32]\n" "stp q4, q5, [x29, #64]\n"
        "stp q6, q7, [x29, #96]\n" "stp q8, q9, [x29, #128]\n" "stp q10, q11, [x29, #160]\n"
        "stp q12, q13, [x29, #192]\n" "stp q14, q15, [x29, #224]\n" "stp q16, q17, [x29, #256]\n"
        "stp q18, q19, [x29, #288]\n" "stp q20, q21, [x29, #320]\n" "stp q22, q23, [x29, #352]\n"
        "stp q24, q25, [x29, #384]\n" "stp q26, q27, [x29, #416]\n" "stp q28, q29, [x29, #448]\n"
        "stp q30, q31, [x29, #480]\n"
        "ldp x29, x30, [sp], #16\n"
        :
        : "r"(block)
        : "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13",
          "x14", "x15", "x16", "x17", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26",
          "x28", "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10", "v11",
          "v12", "v13", "v14", "v15", "v16", "v17", "v18", "v19", "v20", "v21", "v22", "v23",
          "v24", "v25", "v26", "v27", "v28", "v29", "v30", "v31", "memory", "cc");
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 1000;
    uint64_t spins = argc > 2 ? strtoull(argv[2], NULL, 0) : 100000;
    args.x28_mode = argc > 3 && !strcmp(argv[3], "x28");
    long bad = 0;
    for (int r = 0; r < rounds; r++) {
        for (int i = 0; i < NX; i++) expect_x[i] = 0x1000000000000000ULL * (i % 15 + 1) + r * 0x10001ULL + i;
        for (int i = 0; i < NQ * 2; i++) expect_q[i] = 0xA5A5000000000000ULL ^ ((uint64_t)i << 32) ^ (r * 0x9E3779B9ULL);
        memset(seen_x, 0, sizeof(seen_x));
        memset(seen_q, 0, sizeof(seen_q));
        run(spins);
        for (int i = 0; i < NX; i++)
            if (seen_x[i] != expect_x[i] && bad++ < 20)
                fprintf(stderr, "round %d: x%d = %#llx, expected %#llx\n", r, i < 18 ? i : i + 1,
                        (unsigned long long)seen_x[i], (unsigned long long)expect_x[i]);
        for (int i = 0; i < NQ * 2; i++)
            if (seen_q[i] != expect_q[i] && bad++ < 20)
                fprintf(stderr, "round %d: q%d.%s = %#llx, expected %#llx\n", r, i / 2, i % 2 ? "hi" : "lo",
                        (unsigned long long)seen_q[i], (unsigned long long)expect_q[i]);
    }
    printf(bad ? "BAD %ld\n" : "OK\n", bad);
    return 0;
}
