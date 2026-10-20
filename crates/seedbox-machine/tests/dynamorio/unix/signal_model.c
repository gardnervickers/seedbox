/* Source: DR 356c1821045a4b5678d0813e132074324623df6e suite/tests/api/a64_prop_signal.c. External application core; public kernel ABI only. */
#ifndef _GNU_SOURCE
#    define _GNU_SOURCE
#endif

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

#include <stdarg.h>
#include <sched.h>
#include "signal_rng.h"
#include "signal_records.h"

#ifndef HWCAP_DIT
#    define HWCAP_DIT (1 << 24)
#endif
#ifndef HWCAP2_SME
#    define HWCAP2_SME (1 << 23)
#endif
#ifndef PR_SME_GET_VL
#    define PR_SME_GET_VL 64
#endif

#define PSTATE_NZCV 0xf0000000ULL
#define PSTATE_DIT 0x01000000ULL

/***************************************************************************
 * The scenario context, shared with the assembly (fixed offsets).
 */
#define O_IN_X 0
#define O_IN_NZCV 248
#define O_IN_DIT 256
#define O_IN_SP 264
#define O_BODY 272
#define O_HOST_SP 280
#define O_OUT_X 288
#define O_OUT_SP 536
#define O_OUT_NZCV 544
#define O_OUT_DIT 552
#define O_OUT_CONT 560
#define O_FLAG0 568
#define O_FLAG1 576
#define O_FLAG2 584
#define O_STARTED 600
#define O_HAS_DIT 608
#define O_PID 616
#define O_TID 624
#define O_SIG 632
#define O_AUX 640
#define O_AUX2 648
#define O_COUNT 656
#define O_RESULT 664
#define O_SET 672
#define O_PEND 680
#define O_SCRATCH 688
#define O_TGT 704
#define O_IN_RUN 736
#define O_IN_V 768
#define O_OUT_V 1280
#define O_SS_ON 1792
#define O_SS_OFF 1816
#define O_SS_Q 1840
#define O_RTTAB 2048
#define RT_MAX 16

typedef struct {
    uint64_t in_x[31];
    uint64_t in_nzcv, in_dit, in_sp, body, host_sp;
    uint64_t out_x[31];
    uint64_t out_sp, out_nzcv, out_dit, out_cont;
    volatile uint64_t flag[4];
    volatile uint64_t started;
    uint64_t has_dit, pid, tid, sig, aux, aux2, count;
    volatile int64_t result;
    uint64_t set, pend;
    uint8_t scratch[16];
    uint64_t tgt[4];
    volatile uint64_t in_run; /* set by a64ps_run, cleared by a64ps_capture */
    uint64_t pad0[3];
    __uint128_t in_v[32];
    __uint128_t out_v[32];
    stack_t ss_on, ss_off, ss_q;
    uint8_t pad1[2048 - 1864];
    uint8_t rttab[RT_MAX][128];
} sgctx_t;

_Static_assert(offsetof(sgctx_t, in_nzcv) == O_IN_NZCV, "asm offset");
_Static_assert(offsetof(sgctx_t, in_dit) == O_IN_DIT, "asm offset");
_Static_assert(offsetof(sgctx_t, in_sp) == O_IN_SP, "asm offset");
_Static_assert(offsetof(sgctx_t, body) == O_BODY, "asm offset");
_Static_assert(offsetof(sgctx_t, host_sp) == O_HOST_SP, "asm offset");
_Static_assert(offsetof(sgctx_t, out_x) == O_OUT_X, "asm offset");
_Static_assert(offsetof(sgctx_t, out_sp) == O_OUT_SP, "asm offset");
_Static_assert(offsetof(sgctx_t, out_nzcv) == O_OUT_NZCV, "asm offset");
_Static_assert(offsetof(sgctx_t, out_dit) == O_OUT_DIT, "asm offset");
_Static_assert(offsetof(sgctx_t, out_cont) == O_OUT_CONT, "asm offset");
_Static_assert(offsetof(sgctx_t, flag) == O_FLAG0, "asm offset");
_Static_assert(offsetof(sgctx_t, started) == O_STARTED, "asm offset");
_Static_assert(offsetof(sgctx_t, has_dit) == O_HAS_DIT, "asm offset");
_Static_assert(offsetof(sgctx_t, pid) == O_PID, "asm offset");
_Static_assert(offsetof(sgctx_t, tid) == O_TID, "asm offset");
_Static_assert(offsetof(sgctx_t, sig) == O_SIG, "asm offset");
_Static_assert(offsetof(sgctx_t, aux) == O_AUX, "asm offset");
_Static_assert(offsetof(sgctx_t, aux2) == O_AUX2, "asm offset");
_Static_assert(offsetof(sgctx_t, count) == O_COUNT, "asm offset");
_Static_assert(offsetof(sgctx_t, result) == O_RESULT, "asm offset");
_Static_assert(offsetof(sgctx_t, set) == O_SET, "asm offset");
_Static_assert(offsetof(sgctx_t, pend) == O_PEND, "asm offset");
_Static_assert(offsetof(sgctx_t, scratch) == O_SCRATCH, "asm offset");
_Static_assert(offsetof(sgctx_t, tgt) == O_TGT, "asm offset");
_Static_assert(offsetof(sgctx_t, in_run) == O_IN_RUN, "asm offset");
_Static_assert(offsetof(sgctx_t, in_v) == O_IN_V, "asm offset");
_Static_assert(offsetof(sgctx_t, out_v) == O_OUT_V, "asm offset");
_Static_assert(offsetof(sgctx_t, ss_on) == O_SS_ON, "asm offset");
_Static_assert(offsetof(sgctx_t, ss_off) == O_SS_OFF, "asm offset");
_Static_assert(offsetof(sgctx_t, ss_q) == O_SS_Q, "asm offset");
_Static_assert(offsetof(sgctx_t, rttab) == O_RTTAB, "asm offset");

/* Fixed addresses, so that native and DR runs see identical stacks and frames. */
#define SG_BASE 0x3200000000ULL
#define SG_CTX ((sgctx_t *)SG_BASE)
#define SG_STK_LO (SG_BASE + 0x10000ULL)
#define SG_STK_TOP (SG_BASE + 0x30000ULL)
#define SG_ALT_LO (SG_BASE + 0x40000ULL)
#define SG_ALT_SIZE 0x10000ULL
#define SG_GUARD (SG_BASE + 0x50000ULL) /* PROT_NONE: the SEGV scenario's target */
#define SG_MAP_SIZE 0x51000ULL
#define SG_CANARY 64 /* bytes of canaries the bodies push below in_sp */

#define SGS_(x) #x
#define SGS(x) SGS_(x)

/* clang-format off */
/* Every scenario body starts here: canaries (x0..x7) on the stack, x16 = 0,
 * x17 = 1 and ctx->started = 1.
 */
#define PRELUDE                                    \
    "  sub sp, sp, #64\n"                          \
    "  stp x0, x1, [sp]\n"                         \
    "  stp x2, x3, [sp, #16]\n"                    \
    "  stp x4, x5, [sp, #32]\n"                    \
    "  stp x6, x7, [sp, #48]\n"                    \
    "  mov x16, #0\n"                              \
    "  mov x17, #1\n"                              \
    "  str x17, [x28, #" SGS(O_STARTED) "]\n"
#define TGKILL_SELF                                \
    "  ldr x0, [x28, #" SGS(O_PID) "]\n"           \
    "  ldr x1, [x28, #" SGS(O_TID) "]\n"           \
    "  ldr x2, [x28, #" SGS(O_SIG) "]\n"           \
    "  mov x8, #131\n"                             \
    "  svc #0\n"
#define SIGPROCMASK(how)                           \
    "  mov x0, #" #how "\n"                        \
    "  add x1, x28, #" SGS(O_SET) "\n"             \
    "  mov x2, #0\n"                               \
    "  mov x3, #8\n"                               \
    "  mov x8, #135\n"                             \
    "  svc #0\n"

/* x28 holds the context throughout (it is also DR's default stolen register). */
__asm__(".text\n"
        ".p2align 4\n"
        ".global a64ps_code_lo\n"
        "a64ps_code_lo:\n"
        ".global a64ps_run\n"
        ".type a64ps_run, %function\n"
        "a64ps_run:\n"
        "  hint #34\n"
        "  stp x29, x30, [sp, #-160]!\n"
        "  mov x29, sp\n"
        "  stp x19, x20, [sp, #16]\n"
        "  stp x21, x22, [sp, #32]\n"
        "  stp x23, x24, [sp, #48]\n"
        "  stp x25, x26, [sp, #64]\n"
        "  stp x27, x28, [sp, #80]\n"
        "  stp d8, d9, [sp, #96]\n"
        "  stp d10, d11, [sp, #112]\n"
        "  stp d12, d13, [sp, #128]\n"
        "  stp d14, d15, [sp, #144]\n"
        "  mov x28, x0\n"
        "  mov x1, #1\n"
        "  str x1, [x28, #" SGS(O_IN_RUN) "]\n"
        "  mov x1, sp\n"
        "  str x1, [x28, #" SGS(O_HOST_SP) "]\n"
        "  add x16, x28, #" SGS(O_IN_V) "\n"
        "  ldp q0, q1, [x16, #0]\n"
        "  ldp q2, q3, [x16, #32]\n"
        "  ldp q4, q5, [x16, #64]\n"
        "  ldp q6, q7, [x16, #96]\n"
        "  ldp q8, q9, [x16, #128]\n"
        "  ldp q10, q11, [x16, #160]\n"
        "  ldp q12, q13, [x16, #192]\n"
        "  ldp q14, q15, [x16, #224]\n"
        "  ldp q16, q17, [x16, #256]\n"
        "  ldp q18, q19, [x16, #288]\n"
        "  ldp q20, q21, [x16, #320]\n"
        "  ldp q22, q23, [x16, #352]\n"
        "  ldp q24, q25, [x16, #384]\n"
        "  ldp q26, q27, [x16, #416]\n"
        "  ldp q28, q29, [x16, #448]\n"
        "  ldp q30, q31, [x16, #480]\n"
        "  ldr x1, [x28, #" SGS(O_IN_NZCV) "]\n"
        "  msr nzcv, x1\n"
        "  ldr x1, [x28, #" SGS(O_HAS_DIT) "]\n"
        "  cbz x1, 1f\n"
        "  ldr x1, [x28, #" SGS(O_IN_DIT) "]\n"
        "  msr s3_3_c4_c2_5, x1\n" /* dit */
        "1:\n"
        "  ldr x1, [x28, #" SGS(O_IN_SP) "]\n"
        "  mov sp, x1\n"
        "  add x16, x28, #" SGS(O_IN_X) "\n"
        "  ldp x0, x1, [x16, #0]\n"
        "  ldp x2, x3, [x16, #16]\n"
        "  ldp x4, x5, [x16, #32]\n"
        "  ldp x6, x7, [x16, #48]\n"
        "  ldp x8, x9, [x16, #64]\n"
        "  ldp x10, x11, [x16, #80]\n"
        "  ldp x12, x13, [x16, #96]\n"
        "  ldp x14, x15, [x16, #112]\n"
        "  ldr x17, [x16, #136]\n"
        "  ldp x18, x19, [x16, #144]\n"
        "  ldp x20, x21, [x16, #160]\n"
        "  ldp x22, x23, [x16, #176]\n"
        "  ldp x24, x25, [x16, #192]\n"
        "  ldp x26, x27, [x16, #208]\n"
        "  ldp x29, x30, [x16, #232]\n"
        "  ldr x16, [x28, #" SGS(O_BODY) "]\n"
        "  br x16\n"
        /* Final state capture: every body ends here. */
        ".global a64ps_capture\n"
        "a64ps_capture:\n"
        "  str x16, [x28, #" SGS(O_OUT_X) "+128]\n"
        "  str x17, [x28, #" SGS(O_OUT_X) "+136]\n"
        "  add x16, x28, #" SGS(O_OUT_X) "\n"
        "  stp x0, x1, [x16, #0]\n"
        "  stp x2, x3, [x16, #16]\n"
        "  stp x4, x5, [x16, #32]\n"
        "  stp x6, x7, [x16, #48]\n"
        "  stp x8, x9, [x16, #64]\n"
        "  stp x10, x11, [x16, #80]\n"
        "  stp x12, x13, [x16, #96]\n"
        "  stp x14, x15, [x16, #112]\n"
        "  stp x18, x19, [x16, #144]\n"
        "  stp x20, x21, [x16, #160]\n"
        "  stp x22, x23, [x16, #176]\n"
        "  stp x24, x25, [x16, #192]\n"
        "  stp x26, x27, [x16, #208]\n"
        "  str x28, [x16, #224]\n"
        "  stp x29, x30, [x16, #232]\n"
        "  mov x17, sp\n"
        "  str x17, [x28, #" SGS(O_OUT_SP) "]\n"
        "  mrs x17, nzcv\n"
        "  str x17, [x28, #" SGS(O_OUT_NZCV) "]\n"
        "  ldr x17, [x28, #" SGS(O_HAS_DIT) "]\n"
        "  cbz x17, 1f\n"
        "  mrs x17, s3_3_c4_c2_5\n"
        "  str x17, [x28, #" SGS(O_OUT_DIT) "]\n"
        "  msr s3_3_c4_c2_5, xzr\n"
        "1:\n"
        "  add x16, x28, #" SGS(O_OUT_V) "\n"
        "  stp q0, q1, [x16, #0]\n"
        "  stp q2, q3, [x16, #32]\n"
        "  stp q4, q5, [x16, #64]\n"
        "  stp q6, q7, [x16, #96]\n"
        "  stp q8, q9, [x16, #128]\n"
        "  stp q10, q11, [x16, #160]\n"
        "  stp q12, q13, [x16, #192]\n"
        "  stp q14, q15, [x16, #224]\n"
        "  stp q16, q17, [x16, #256]\n"
        "  stp q18, q19, [x16, #288]\n"
        "  stp q20, q21, [x16, #320]\n"
        "  stp q22, q23, [x16, #352]\n"
        "  stp q24, q25, [x16, #384]\n"
        "  stp q26, q27, [x16, #416]\n"
        "  stp q28, q29, [x16, #448]\n"
        "  stp q30, q31, [x16, #480]\n"
        "  ldr x17, [x28, #" SGS(O_HOST_SP) "]\n"
        "  mov sp, x17\n"
        "  str xzr, [x28, #" SGS(O_IN_RUN) "]\n"
        "  ldp x19, x20, [sp, #16]\n"
        "  ldp x21, x22, [sp, #32]\n"
        "  ldp x23, x24, [sp, #48]\n"
        "  ldp x25, x26, [sp, #64]\n"
        "  ldp x27, x28, [sp, #80]\n"
        "  ldp d8, d9, [sp, #96]\n"
        "  ldp d10, d11, [sp, #112]\n"
        "  ldp d12, d13, [sp, #128]\n"
        "  ldp d14, d15, [sp, #144]\n"
        "  ldp x29, x30, [sp], #160\n"
        "  ret\n"
        /* Synchronous: UDF / guard-page load / BRK at a labelled pc.  The
         * continuations record which way the body left in out_cont (x17).
         */
        "a64ps_body_udf:\n" PRELUDE
        ".global a64ps_udf\n"
        "a64ps_udf:\n"
        "  .inst 0x00000051\n" /* udf #0x51 */
        "  b a64ps_next\n"
        "a64ps_body_segv:\n" PRELUDE
        "  ldr x16, [x28, #" SGS(O_AUX2) "]\n"
        ".global a64ps_segv\n"
        "a64ps_segv:\n"
        "  ldr x16, [x16]\n"
        "  b a64ps_next\n"
        "a64ps_body_brk:\n" PRELUDE
        ".global a64ps_brk\n"
        "a64ps_brk:\n"
        "  brk #0x51\n"
        ".global a64ps_next\n"
        "a64ps_next:\n"
        "  mov x17, #0\n"
        "  b a64ps_exit\n"
        ".global a64ps_cont1\n"
        "a64ps_cont1:\n"
        "  mov x17, #1\n"
        "  b a64ps_exit\n"
        ".global a64ps_cont2\n"
        "a64ps_cont2:\n"
        "  mov x17, #2\n"
        "  nop\n"
        "  b a64ps_exit\n"
        "a64ps_exit:\n"
        "  str x17, [x28, #" SGS(O_OUT_CONT) "]\n"
        "  b a64ps_capture\n"
        /* Asynchronous: raised by the block's own svc, or by another source,
         * then the block spins until the handler sets flag0.
         */
        "a64ps_body_self:\n" PRELUDE TGKILL_SELF
        ".global a64ps_self_after\n"
        "a64ps_self_after:\n"
        "  b a64ps_spin\n"
        "a64ps_body_ext:\n" PRELUDE
        ".global a64ps_spin\n"
        "a64ps_spin:\n"
        "  ldr x16, [x28, #" SGS(O_FLAG0) "]\n"
        "  cbz x16, a64ps_spin\n"
        "  b a64ps_next\n"
        /* The same with indirect calls and returns in the loop (IBL churn). */
        "a64ps_body_ispin:\n" PRELUDE
        ".global a64ps_ispin\n"
        "a64ps_ispin:\n"
        "  ldr x16, [x28, #" SGS(O_FLAG0) "]\n"
        "  cbnz x16, a64ps_next\n"
        "  add x17, x17, #1\n"
        "  and x16, x17, #3\n"
        "  add x16, x28, x16, lsl #3\n"
        "  ldr x16, [x16, #" SGS(O_TGT) "]\n"
        "  blr x16\n"
        ".global a64ps_ispin_ret\n"
        "a64ps_ispin_ret:\n"
        "  b a64ps_ispin\n"
        ".global a64ps_tgt0\n"
        "a64ps_tgt0:\n"
        "  ret\n"
        ".global a64ps_tgt1\n"
        "a64ps_tgt1:\n"
        "  nop\n"
        "  ret\n"
        ".global a64ps_tgt2\n"
        "a64ps_tgt2:\n"
        "  b a64ps_tgt0\n"
        ".global a64ps_tgt3\n"
        "a64ps_tgt3:\n"
        "  mov x16, x30\n"
        "  br x16\n"
        ".global a64ps_ispin_end\n"
        "a64ps_ispin_end:\n"
        /* sigprocmask inside the block: block, raise, wait, record the pending
         * set and the handler count so far, unblock, wait for the handler.
         */
        "a64ps_body_mask:\n" PRELUDE SIGPROCMASK(0) TGKILL_SELF
        "  mov x9, #4000\n"
        "1:\n"
        "  sub x9, x9, #1\n"
        "  cbnz x9, 1b\n"
        "  add x0, x28, #" SGS(O_PEND) "\n"
        "  mov x1, #8\n"
        "  mov x8, #136\n"
        "  svc #0\n"
        "  ldr x9, [x28, #" SGS(O_FLAG0) "]\n"
        "  str x9, [x28, #" SGS(O_RESULT) "]\n" SIGPROCMASK(1)
        ".global a64ps_mask_spin\n"
        "a64ps_mask_spin:\n"
        "  ldr x16, [x28, #" SGS(O_FLAG0) "]\n"
        "  cbz x16, a64ps_mask_spin\n"
        "  b a64ps_next\n"
        /* sigaltstack enabled, signal, disabled, signal, queried. */
        "a64ps_body_alt:\n" PRELUDE
        "  add x0, x28, #" SGS(O_SS_ON) "\n"
        "  mov x1, #0\n"
        "  mov x8, #132\n"
        "  svc #0\n" TGKILL_SELF
        ".global a64ps_alt_spin1\n"
        "a64ps_alt_spin1:\n"
        "  ldr x16, [x28, #" SGS(O_FLAG0) "]\n"
        "  cbz x16, a64ps_alt_spin1\n"
        "  add x0, x28, #" SGS(O_SS_OFF) "\n"
        "  mov x1, #0\n"
        "  mov x8, #132\n"
        "  svc #0\n"
        "  str x0, [x28, #" SGS(O_RESULT) "]\n" TGKILL_SELF
        ".global a64ps_alt_spin2\n"
        "a64ps_alt_spin2:\n"
        "  ldr x16, [x28, #" SGS(O_FLAG1) "]\n"
        "  cbz x16, a64ps_alt_spin2\n"
        "  mov x0, #0\n"
        "  add x1, x28, #" SGS(O_SS_Q) "\n"
        "  mov x8, #132\n"
        "  svc #0\n"
        "  b a64ps_next\n"
        /* Real-time payloads: block, queue count entries, record pending,
         * unblock, wait until the handler saw all of them.
         */
        "a64ps_body_rt:\n" PRELUDE SIGPROCMASK(0)
        "  ldr x9, [x28, #" SGS(O_COUNT) "]\n"
        "  add x10, x28, #" SGS(O_RTTAB) "\n"
        "1:\n"
        "  cbz x9, 2f\n"
        "  ldr x0, [x28, #" SGS(O_PID) "]\n"
        "  ldr x1, [x28, #" SGS(O_TID) "]\n"
        "  ldr w2, [x10]\n"
        "  mov x3, x10\n"
        "  mov x8, #240\n"
        "  svc #0\n"
        "  add x10, x10, #128\n"
        "  sub x9, x9, #1\n"
        "  b 1b\n"
        "2:\n"
        "  add x0, x28, #" SGS(O_PEND) "\n"
        "  mov x1, #8\n"
        "  mov x8, #136\n"
        "  svc #0\n" SIGPROCMASK(1)
        ".global a64ps_rt_spin\n"
        "a64ps_rt_spin:\n"
        "  ldr x16, [x28, #" SGS(O_FLAG0) "]\n"
        "  cbz x16, a64ps_rt_spin\n"
        "  b a64ps_next\n"
        /* A blocking read of an empty pipe. */
        "a64ps_body_pipe:\n" PRELUDE
        "  ldr x0, [x28, #" SGS(O_AUX) "]\n"
        "  add x1, x28, #" SGS(O_SCRATCH) "\n"
        "  mov x2, #1\n"
        "  mov x8, #63\n"
        ".global a64ps_pipe_svc\n"
        "a64ps_pipe_svc:\n"
        "  svc #0\n"
        "  str x0, [x28, #" SGS(O_RESULT) "]\n"
        "  b a64ps_next\n"
        /* Called from a handler: tgkill(x0, x1, x2), then wait (at most x4
         * iterations) for *x3 to become nonzero.  Returns {*x3, x5}: a nested
         * handler sets x5 in this context.
         */
        ".global a64ps_raise_wait\n"
        ".type a64ps_raise_wait, %function\n"
        "a64ps_raise_wait:\n"
        "  hint #34\n"
        "  mov x5, #0\n"
        "  mov x6, #0\n"
        "  mov x8, #131\n"
        "  svc #0\n"
        ".global a64ps_rw_after\n"
        "a64ps_rw_after:\n"
        "  ldr x6, [x3]\n"
        "  cbnz x6, 1f\n"
        "  sub x4, x4, #1\n"
        "  cbnz x4, a64ps_rw_after\n"
        "1:\n"
        "  mov x0, x6\n"
        "  mov x1, x5\n"
        "  ret\n"
        ".global a64ps_rw_end\n"
        "a64ps_rw_end:\n"
        ".global a64ps_code_hi\n"
        "a64ps_code_hi:\n"
        ".section .data\n"
        ".p2align 3\n"
        "a64ps_bodies:\n"
        "  .quad a64ps_body_udf, a64ps_body_brk, a64ps_body_self, a64ps_body_ext\n"
        "  .quad a64ps_body_ispin, a64ps_body_mask, a64ps_body_alt, a64ps_body_rt\n"
        "  .quad a64ps_body_pipe, a64ps_body_segv\n"
        ".global a64ps_bodies\n"
        ".text\n");
/* clang-format on */

extern const char a64ps_code_lo[], a64ps_code_hi[], a64ps_udf[], a64ps_brk[],
    a64ps_segv[], a64ps_next[], a64ps_cont1[], a64ps_cont2[], a64ps_self_after[],
    a64ps_spin[], a64ps_ispin[], a64ps_ispin_ret[], a64ps_ispin_end[], a64ps_tgt0[],
    a64ps_tgt1[], a64ps_tgt2[], a64ps_tgt3[], a64ps_mask_spin[], a64ps_alt_spin1[],
    a64ps_alt_spin2[], a64ps_rt_spin[], a64ps_pipe_svc[], a64ps_rw_after[],
    a64ps_rw_end[];
extern const uint64_t a64ps_bodies[];
void
a64ps_run(sgctx_t *ctx);
typedef struct {
    uint64_t flag, mark;
} rw_ret_t;
rw_ret_t
a64ps_raise_wait(uint64_t pid, uint64_t tid, uint64_t sig, volatile uint64_t *flag,
                 uint64_t iters);

#define PC(sym) ((uint64_t)(uintptr_t)(sym))

/***************************************************************************
 * Scenario kinds and their parameters.
 */
enum {
    K_UDF,    /* synchronous SIGILL at a labelled pc */
    K_BRK,    /* synchronous SIGTRAP */
    K_SEGV,   /* synchronous SIGSEGV (guard page): ESR record and fault address */
    K_SELF,   /* raised by the block's svc, then a spin */
    K_EXT,    /* another thread's tgkill/kill, itimer or POSIX timer, during a spin */
    K_NEST,   /* the handler raises a second signal (SA_NODEFER or another one) */
    K_SAMASK, /* sa_mask blocks a signal the handler raises until sigreturn */
    K_MASK,   /* sigprocmask inside the block */
    K_ALT,    /* sigaltstack enabled and disabled inside the block */
    K_RT,     /* real-time payloads and order */
    K_PIPE,   /* blocking read interrupted, SA_RESTART on or off */
    K_ISPIN,  /* external signal into a spin with indirect calls (paths mode) */
    K_NUM
};
static const char *const kind_name[K_NUM] = { "udf", "brk",  "segv",   "self",
                                              "ext", "nest", "samask", "mask",
                                              "alt", "rt",   "pipe",   "ispin" };
/* Index into a64ps_bodies. */
static const int kind_body[K_NUM] = { 0, 1, 9, 2, 3, 2, 2, 5, 6, 7, 8, 4 };

enum { SRC_SELF, SRC_TGKILL, SRC_KILL, SRC_ITIMER, SRC_PTIMER };
static const char *const src_name[] = { "self", "tgkill", "kill", "itimer", "ptimer" };

enum {
    EFF_GPR = 1,
    EFF_NZCV = 2,
    EFF_DIT = 4,
    EFF_PC = 8,
    EFF_SP = 16,
    EFF_V = 32,
};

typedef struct {
    int kind, idx;
    uint64_t seed;
    int sig, sig2, src, restart, nodefer, onstack;
    uint64_t samask; /* extra signals in the handler's sa_mask */
    unsigned eff;
    int gpr, cont, vreg;
    uint64_t gval, nzcv, dit;
    int64_t spdelta;
    __uint128_t vval;
    int nrt, rt_masked;
    int rt_sig[RT_MAX], rt_val[RT_MAX];
    int nexpect; /* rt: deliveries expected */
} sgcase_t;

/* What a run observed: compared between native and DR, and against the model. */
#define NOBS 24
#define NLOG 24
typedef struct {
    int valid; /* the run completed */
    int conclusive;
    uint64_t out_x[31], out_sp, out_nzcv, out_dit, out_cont;
    __uint128_t out_v[32];
    uint64_t layout; /* record layout of the first handler frame */
    int nobs;
    uint64_t obs[NOBS];
    int nlog;
    uint64_t log[NLOG];
} sgres_t;

/***************************************************************************
 * Global run state (the handlers' view of the current case).
 */
typedef struct {
    sgcase_t *c;
    sgres_t *r;
    volatile int running, depth;
    /* The modelled context at the first (and second) interruption. */
    uint64_t mx[31], msp, mnzcv, mdit;
    __uint128_t mv[32];
    uint64_t fpcr;
    int phase, outer_done, nest_phase;
    uint64_t outer_mask;
    int nrt_log;
    /* Failures inside handlers: counted, the first one described. */
    int fails;
    char fail_msg[512];
    long ctx_checked, eff_applied, frames_ok, strays;
    volatile long other_thread;
    int sve_vl, za_vl;
    long native_za_vl;
} sgrun_t;
static sgrun_t g;

/* One entry-frame observation for UDF, independent of per-case run state. */
static struct a64p_frame_snapshot first_udf_frame;
static int first_udf_index = -1;
static uint64_t first_udf_seed, first_udf_sp_distance;
static int first_udf_on_alt, first_udf_expected_alt, first_udf_below_sp;

static int verbose;
static int g_is_dr;
static long model_stat[5];

/* Counters a child reports to the parent through the progress page. */
enum {
    ST_CTX_CHECKED,
    ST_EFF_APPLIED,
    ST_FRAMES_OK,
    ST_STRAYS,
    ST_KD_RT_ORDER, /* known deviation rt-overflow-order (see below) */
};

/* Known deviation rt-overflow-order (2026-10-14): DR queues at most
 * -max_pending_signals (8) signals per thread and hands a further one back to the
 * kernel with rt_tgsigqueueinfo, which puts it behind the later instances of the
 * same real-time signal still pending there; so when more than 8 distinct
 * signals become deliverable at once, one real-time signal's first instance can
 * be delivered after its later ones.  Every payload must still arrive exactly
 * once; the order difference is counted.
 */
#define DR_MAX_PENDING_SIGNALS 8
static int is_debug_build;
static int has_dit;
static pid_t main_tid;
static int pipe_fd[2] = { -1, -1 };

/* Source tolerated RT overflow reordering. External regression ports require
 * the complete model, so that tolerance is intentionally not carried over. */
static int rt_overflow_deviation(const sgcase_t *c) { (void)c;return 0; }

static int sig_pool[8];

static uint64_t
now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static void
hfail(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void
hfail(const char *fmt, ...)
{
    if (g.fails++ == 0) {
        va_list ap;
        va_start(ap, fmt);
        vsnprintf(g.fail_msg, sizeof(g.fail_msg), fmt, ap);
        va_end(ap);
    }
}

static void
add_obs(uint64_t v)
{
    if (g.r->nobs < NOBS)
        g.r->obs[g.r->nobs++] = v;
}

/***************************************************************************
 * Case generation.
 */
static const int rt_choices = 8;

static void
gen_case(int kind, int idx, uint64_t seed, sgcase_t *c, int paths)
{
    rng_t r = { seed * 0x2545f4914f6cdd1dULL + (uint64_t)kind * 0x9e3779b97f4a7c15ULL };
    memset(c, 0, sizeof(*c));
    c->kind = kind;
    c->idx = idx;
    c->seed = seed;
    c->sig = sig_pool[rn(&r, 8)];
    do
        c->sig2 = sig_pool[rn(&r, 8)];
    while (c->sig2 == c->sig);
    c->src = SRC_SELF;
    c->onstack = rp(&r, 30);
    c->restart = rp(&r, 50);
    if (kind == K_EXT || kind == K_ISPIN) {
        c->src = SRC_TGKILL + (int)rn(&r, 4);
        if (c->src == SRC_ITIMER)
            c->sig = SIGALRM;
        else if (c->src == SRC_PTIMER)
            c->sig = SIGRTMIN + 9;
    }
    /* Extra signals blocked during the handler (never the ones the case waits for). */
    for (int i = 0; i < 8; i++) {
        if (sig_pool[i] != c->sig && sig_pool[i] != c->sig2 && rp(&r, 30))
            c->samask |= 1ULL << (sig_pool[i] - 1);
    }
    unsigned allowed = EFF_GPR | EFF_NZCV | EFF_DIT | EFF_PC | EFF_SP | EFF_V;
    if (kind == K_ALT || kind == K_PIPE || kind == K_SAMASK)
        allowed &= ~(EFF_PC | EFF_SP);
    if (kind == K_RT)
        allowed = 0;
    for (int b = 1; b <= EFF_V; b <<= 1) {
        if ((allowed & b) && rp(&r, 50))
            c->eff |= b;
    }
    static const int gprs[] = { 3,  4,  5,  6,  7,  9,  10, 11, 12, 13, 14, 15,
                                18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 29, 30 };
    int ng = (int)(sizeof(gprs) / sizeof(gprs[0]));
    if (kind == K_ISPIN)
        ng--; /* x30 is the loop's link register */
    c->gpr = gprs[rn(&r, ng)];
    c->gval = rval(&r);
    c->nzcv = (uint64_t)rn(&r, 16) << 28;
    c->dit = rp(&r, 50) ? PSTATE_DIT : 0;
    c->cont = 1 + (int)rn(&r, 2);
    static const int64_t deltas[] = { -64, -48, -32, -16, 16, 32, 48, 64 };
    c->spdelta = deltas[rn(&r, 8)];
    c->vreg = (int)rn(&r, 32);
    c->vval = ((__uint128_t)rnext(&r) << 64) | rnext(&r);
    c->nodefer = rp(&r, 50);
    if (kind == K_NEST && c->nodefer)
        c->sig2 = c->sig;
    if (kind == K_RT) {
        c->nrt = 3 + (int)rn(&r, RT_MAX - 3);
        c->rt_masked = rp(&r, 60);
        uint64_t std_seen = 0;
        for (int i = 0; i < c->nrt; i++) {
            int k = (int)rn(&r, rt_choices + 2);
            c->rt_sig[i] = k < rt_choices ? SIGRTMIN + 3 + k
                : k == rt_choices         ? SIGUSR1
                                          : SIGUSR2;
            c->rt_val[i] = (int)(rnext(&r) & 0x7fffffff);
            if (c->rt_sig[i] < SIGRTMIN) {
                /* Standard signals do not queue: only one is delivered. */
                if (!(std_seen & (1ULL << c->rt_sig[i])))
                    c->nexpect++;
                std_seen |= 1ULL << c->rt_sig[i];
            } else
                c->nexpect++;
        }
    }
    (void)paths;
}

/***************************************************************************
 * The model.
 */
static int
kind_effects_once(const sgcase_t *c)
{
    return c->kind != K_RT;
}

/* The modelled register state where the case's (first) signal is taken. */
static void
model_first(const sgcase_t *c, sgctx_t *x)
{
    memcpy(g.mx, x->in_x, sizeof(g.mx));
    g.mx[28] = (uint64_t)(uintptr_t)x;
    g.mx[16] = 0;
    g.mx[17] = 1;
    g.msp = x->in_sp - SG_CANARY;
    g.mnzcv = x->in_nzcv;
    g.mdit = x->in_dit;
    memcpy(g.mv, x->in_v, sizeof(g.mv));
    switch (c->kind) {
    case K_SELF:
    case K_NEST:
    case K_SAMASK:
    case K_ALT:
        g.mx[0] = 0;
        g.mx[1] = x->tid;
        g.mx[2] = x->sig;
        g.mx[8] = 131;
        break;
    case K_MASK:
    case K_RT:
        g.mx[0] = 0;
        g.mx[1] = (uint64_t)(uintptr_t)x + O_SET;
        g.mx[2] = 0;
        g.mx[3] = 8;
        g.mx[8] = 135;
        g.mx[9] = 0;
        if (c->kind == K_RT)
            g.mx[10] = (uint64_t)(uintptr_t)x + O_RTTAB + 128ULL * (uint64_t)c->nrt;
        break;
    case K_PIPE:
        g.mx[0] = c->restart ? x->aux : (uint64_t)-EINTR;
        g.mx[1] = (uint64_t)(uintptr_t)x + O_SCRATCH;
        g.mx[2] = 1;
        g.mx[8] = 63;
        break;
    default: break;
    }
}

/* Applies the case's effects to the model (what the handler writes). */
static void
model_apply(const sgcase_t *c)
{
    if (c->eff & EFF_GPR)
        g.mx[c->gpr] = c->gval;
    if (c->eff & EFF_NZCV)
        g.mnzcv = c->nzcv;
    if (c->eff & EFF_DIT)
        g.mdit = c->dit;
    if (c->eff & EFF_SP)
        g.msp += (uint64_t)c->spdelta;
    if (c->eff & EFF_V)
        g.mv[c->vreg] = c->vval;
}

/* Whether pc is where the case's primary signal may be taken. */
static int
pc_allowed(const sgcase_t *c, uint64_t pc, int phase)
{
    switch (c->kind) {
    case K_UDF: return pc == PC(a64ps_udf);
    case K_BRK: return pc == PC(a64ps_brk);
    case K_SEGV: return pc == PC(a64ps_segv);
    case K_SELF:
    case K_NEST:
    case K_SAMASK:
        return pc == PC(a64ps_self_after) || pc == PC(a64ps_spin) ||
            pc == PC(a64ps_spin) + 4;
    case K_EXT: return pc == PC(a64ps_spin) || pc == PC(a64ps_spin) + 4;
    case K_ISPIN: return pc >= PC(a64ps_ispin) && pc < PC(a64ps_ispin_end);
    case K_MASK: return pc == PC(a64ps_mask_spin) || pc == PC(a64ps_mask_spin) + 4;
    case K_ALT: {
        uint64_t s = phase == 0 ? PC(a64ps_alt_spin1) : PC(a64ps_alt_spin2);
        return pc == s || pc == s + 4;
    }
    case K_RT: return pc == PC(a64ps_rt_spin) || pc == PC(a64ps_rt_spin) + 4;
    case K_PIPE: return pc == PC(a64ps_pipe_svc) + (c->restart ? 0 : 4);
    }
    return 0;
}

/* Compares an interrupted context with the model.  x16 and x17 are the spin
 * loops' scratch registers; in the ISPIN loop x30 is its link register.
 */
static void
check_ctx(const char *what, ucontext_t *uc, struct a64p_fpsimd_rec *fp)
{
    const sgcase_t *c = g.c;
    int bad = 0;
    char buf[256];
    buf[0] = '\0';
    for (int i = 0; i < 31; i++) {
        if (i == 16 || i == 17 || (i == 30 && c->kind == K_ISPIN))
            continue;
        if (uc->uc_mcontext.regs[i] != g.mx[i]) {
            if (bad++ == 0) {
                snprintf(buf, sizeof(buf), "x%d 0x%llx, model 0x%llx", i,
                         (unsigned long long)uc->uc_mcontext.regs[i],
                         (unsigned long long)g.mx[i]);
            }
        }
    }
    if (c->kind == K_ISPIN && uc->uc_mcontext.regs[30] != g.mx[30] &&
        uc->uc_mcontext.regs[30] != PC(a64ps_ispin_ret)) {
        if (bad++ == 0) {
            snprintf(buf, sizeof(buf), "x30 0x%llx",
                     (unsigned long long)uc->uc_mcontext.regs[30]);
        }
    }
    if (uc->uc_mcontext.sp != g.msp && bad++ == 0) {
        snprintf(buf, sizeof(buf), "sp 0x%llx, model 0x%llx",
                 (unsigned long long)uc->uc_mcontext.sp, (unsigned long long)g.msp);
    }
    uint64_t ps = uc->uc_mcontext.pstate;
    if ((ps & PSTATE_NZCV) != g.mnzcv && bad++ == 0) {
        snprintf(buf, sizeof(buf), "pstate 0x%llx: nzcv, model 0x%llx",
                 (unsigned long long)ps, (unsigned long long)g.mnzcv);
    }
    if (has_dit && (ps & PSTATE_DIT) != g.mdit && bad++ == 0) {
        snprintf(buf, sizeof(buf), "pstate 0x%llx: DIT, model 0x%llx",
                 (unsigned long long)ps, (unsigned long long)g.mdit);
    }
    if (fp == NULL) {
        if (bad++ == 0)
            snprintf(buf, sizeof(buf), "no FPSIMD record");
    } else {
        for (int i = 0; i < 32; i++) {
            if (fp->v[i] != g.mv[i] && bad++ == 0) {
                snprintf(
                    buf, sizeof(buf), "v%d 0x%016llx%016llx, model 0x%016llx%016llx", i,
                    (unsigned long long)(fp->v[i] >> 64), (unsigned long long)fp->v[i],
                    (unsigned long long)(g.mv[i] >> 64), (unsigned long long)g.mv[i]);
            }
        }
        if (fp->fpcr != (uint32_t)g.fpcr && bad++ == 0)
            snprintf(buf, sizeof(buf), "fpcr 0x%x, host 0x%llx", fp->fpcr,
                     (unsigned long long)g.fpcr);
    }
    if (bad > 0) {
        hfail("%s: interrupted context at pc 0x%llx (+0x%llx) differs from the model in "
              "%d fields; first: %s",
              what, (unsigned long long)uc->uc_mcontext.pc,
              (unsigned long long)(uc->uc_mcontext.pc - PC(a64ps_code_lo)), bad, buf);
    } else
        g.ctx_checked++;
}

/* Writes the case's effects into the frame.  The PC effect is a labelled
 * continuation; the SP effect moves SP within the live stack.
 */
static void
apply_effects(ucontext_t *uc, struct a64p_fpsimd_rec *fp)
{
    const sgcase_t *c = g.c;
    if (c->eff & EFF_GPR)
        uc->uc_mcontext.regs[c->gpr] = c->gval;
    if (c->eff & EFF_NZCV)
        uc->uc_mcontext.pstate = (uc->uc_mcontext.pstate & ~PSTATE_NZCV) | c->nzcv;
    if (has_dit && (c->eff & EFF_DIT))
        uc->uc_mcontext.pstate = (uc->uc_mcontext.pstate & ~PSTATE_DIT) | c->dit;
    if (c->eff & EFF_SP)
        uc->uc_mcontext.sp += (uint64_t)c->spdelta;
    if (c->eff & EFF_PC)
        uc->uc_mcontext.pc = c->cont == 1 ? PC(a64ps_cont1) : PC(a64ps_cont2);
    if ((c->eff & EFF_V) && fp != NULL)
        fp->v[c->vreg] = c->vval;
    g.eff_applied++;
}

/* Validates the frame's records and placement; returns the FPSIMD record. */
static struct a64p_fpsimd_rec *
check_frame(int sig, ucontext_t *uc, int expect_alt)
{
    char err[200];
    uint64_t layout;
    int sve_vl, za_vl;
    struct a64p_frame_snapshot *snapshot = NULL;
    if (sig == SIGILL && g.c != NULL && g.c->kind == K_UDF &&
        uc->uc_mcontext.pc == PC(a64ps_udf) && !first_udf_frame.attempted) {
        snapshot = &first_udf_frame;
        first_udf_index = g.c->idx;
        first_udf_seed = g.c->seed;
        uintptr_t frame = (uintptr_t)uc, sp = uc->uc_mcontext.sp;
        first_udf_on_alt = frame >= SG_ALT_LO && frame < SG_ALT_LO + SG_ALT_SIZE;
        first_udf_expected_alt = expect_alt;
        first_udf_below_sp = frame <= sp;
        first_udf_sp_distance = frame <= sp ? sp - frame : frame - sp;
    }
    if (a64p_check_frame_records_snapshot(uc, &layout, &sve_vl, &za_vl,
                                          err, sizeof(err), snapshot) != 0) {
        hfail("signal %d: invalid frame records: %s", sig, err);
        return NULL;
    }
    if (g.native_za_vl > 0 && za_vl != g.native_za_vl)
        hfail("signal %d: ZA record vl %d, PR_SME_GET_VL %ld", sig, za_vl,
              g.native_za_vl);
    if (g.r != NULL && g.r->layout == 0)
        g.r->layout = layout;
    g.sve_vl = sve_vl;
    g.za_vl = za_vl;
    uint64_t f = (uint64_t)(uintptr_t)uc;
    if (expect_alt >= 0) {
        int on_alt = f >= SG_ALT_LO && f < SG_ALT_LO + SG_ALT_SIZE;
        uint64_t isp = uc->uc_mcontext.sp;
        if (expect_alt && !on_alt)
            hfail("signal %d: frame at 0x%llx, not on the alternate stack", sig,
                  (unsigned long long)f);
        else if (!expect_alt && (f >= isp || f < isp - 0x8000))
            hfail("signal %d: frame at 0x%llx, not just below the interrupted sp 0x%llx",
                  sig, (unsigned long long)f, (unsigned long long)isp);
    }
    g.frames_ok++;
    return (struct a64p_fpsimd_rec *)a64p_find_rec(uc, FPSIMD_MAGIC_);
}

static uint64_t
cur_mask(void)
{
    sigset_t s;
    pthread_sigmask(SIG_BLOCK, NULL, &s);
    uint64_t m = 0;
    for (int i = 1; i <= 64; i++) {
        if (sigismember(&s, i))
            m |= 1ULL << (i - 1);
    }
    return m;
}

static uint64_t
uc_mask(ucontext_t *uc)
{
    uint64_t m;
    memcpy(&m, &uc->uc_sigmask, sizeof(m));
    return m;
}

/***************************************************************************
 * Handlers.
 */
static void
h_sync(int sig, siginfo_t *si, void *ucv)
{
    ucontext_t *uc = (ucontext_t *)ucv;
    sgcase_t *c = g.c;
    uint64_t pc = uc->uc_mcontext.pc;
    if (!g.running || c == NULL ||
        (c->kind != K_UDF && c->kind != K_BRK && c->kind != K_SEGV)) {
        char buf[160];
        int len = snprintf(buf, sizeof(buf),
                           "a64_prop_signal: unexpected signal %d at pc 0x%llx addr %p\n",
                           sig, (unsigned long long)pc, si->si_addr);
        if (write(2, buf, len) < 0) {
            /* Nothing to do. */
        }
        signal(sig, SIG_DFL);
        return;
    }
    g.depth++;
    struct a64p_fpsimd_rec *fp = check_frame(sig, uc, c->onstack);
    if (!pc_allowed(c, pc, 0) || SG_CTX->flag[0] != 0) {
        hfail("synchronous signal %d at pc 0x%llx (flag %llu)", sig,
              (unsigned long long)pc, (unsigned long long)SG_CTX->flag[0]);
        uc->uc_mcontext.pc = PC(a64ps_next);
    } else {
        check_ctx("sync", uc, fp);
        add_obs((uint64_t)si->si_code);
        add_obs((uint64_t)(uintptr_t)si->si_addr);
        add_obs(uc->uc_mcontext.fault_address);
        struct a64p_esr_rec *er = (struct a64p_esr_rec *)a64p_find_rec(uc, ESR_MAGIC_);
        add_obs(er == NULL ? ~0ULL : er->esr);
        if (c->kind == K_SEGV &&
            ((uint64_t)(uintptr_t)si->si_addr != SG_GUARD || er == NULL ||
             (er->esr >> 26) != 0x24)) {
            hfail("segv: si_addr %p, ESR record %s 0x%llx", si->si_addr,
                  er == NULL ? "missing" : "present",
                  er == NULL ? 0ULL : (unsigned long long)er->esr);
        }
        apply_effects(uc, fp);
        if (!(c->eff & EFF_PC))
            uc->uc_mcontext.pc += 4;
        SG_CTX->flag[0] = 1;
    }
    g.depth--;
}

static void
h_async(int sig, siginfo_t *si, void *ucv)
{
    ucontext_t *uc = (ucontext_t *)ucv;
    sgctx_t *x = SG_CTX;
    sgcase_t *c = g.c;
    /* A process-directed signal (kill, itimer) may go to any thread that does
     * not block it, e.g. the helper while glibc's start_thread still runs it
     * with its creator's mask: not the case's.
     */
    if (syscall(SYS_gettid) != main_tid) {
        __atomic_add_fetch(&g.other_thread, 1, __ATOMIC_RELAXED);
        return;
    }
    int depth = ++g.depth;
    uint64_t pc = uc->uc_mcontext.pc;
    /* Outside a64ps_run's span (in_run) the thread runs host code; a signal
     * delivered there (possibly one that arrived in the scenario but whose
     * delivery DR legitimately deferred past its end) is not the case's.
     */
    if (!g.running || c == NULL || (depth == 1 && !x->in_run)) {
        g.strays++;
        goto out;
    }
    int in_code = pc >= PC(a64ps_code_lo) && pc < PC(a64ps_code_hi);
    /* Real-time signals whose handlers do not mask each other nest: the kernel
     * takes the next one at the previous handler's entry.
     */
    int rt_nesting = c->kind == K_RT && !c->rt_masked;
    /* A signal can arrive while a previous handler returns through the vDSO's
     * sigreturn trampoline.
     */
    uint64_t vdso = getauxval(AT_SYSINFO_EHDR);
    int in_vdso = vdso != 0 && pc >= vdso && pc < vdso + 0x10000;
    if (depth == 1 && !in_code && in_vdso) {
        g.strays++;
        goto out;
    }
    if (depth == 1 && !in_code && !rt_nesting) {
        /* While running, the thread only executes the scenario code. */
        Dl_info di;
        int have = dladdr((void *)(uintptr_t)pc, &di) != 0;
        hfail("signal %d: impossible context: pc 0x%llx outside the scenario code "
              "(%s+0x%llx, %s+0x%llx; sp 0x%llx, x30 0x%llx, tid %s, interrupted mask "
              "0x%llx)",
              sig, (unsigned long long)pc,
              have && di.dli_sname != NULL ? di.dli_sname : "?",
              have && di.dli_saddr != NULL
                  ? (unsigned long long)(pc - (uint64_t)(uintptr_t)di.dli_saddr)
                  : 0ULL,
              have && di.dli_fname != NULL ? di.dli_fname : "?",
              have ? (unsigned long long)(pc - (uint64_t)(uintptr_t)di.dli_fbase) : 0ULL,
              (unsigned long long)uc->uc_mcontext.sp,
              (unsigned long long)uc->uc_mcontext.regs[30],
              syscall(SYS_gettid) == main_tid ? "main" : "other",
              (unsigned long long)uc_mask(uc));
        goto out;
    }
    int on_alt_expected = c->onstack;
    switch (c->kind) {
    case K_SELF:
    case K_EXT:
    case K_ISPIN:
    case K_MASK:
    case K_PIPE: {
        if (sig != c->sig || x->flag[0] != 0 || depth != 1) {
            g.strays++;
            break;
        }
        if (!pc_allowed(c, pc, 0)) {
            if (c->kind == K_EXT || c->kind == K_ISPIN) {
                /* Before the block reached its spin (a timer): not the case's signal. */
                g.strays++;
                break;
            }
            hfail(
                "signal %d delivered at pc +0x%llx, where the scenario does not allow it",
                sig, (unsigned long long)(pc - PC(a64ps_code_lo)));
            x->flag[0] = 0xdead;
            break;
        }
        struct a64p_fpsimd_rec *fp = check_frame(sig, uc, on_alt_expected);
        check_ctx(kind_name[c->kind], uc, fp);
        add_obs((uint64_t)si->si_code);
        add_obs(si->si_code == SI_TKILL || si->si_code == SI_USER
                    ? (uint64_t)(si->si_pid == getpid())
                    : 2);
        add_obs(uc_mask(uc));
        if (c->kind == K_PIPE) {
            add_obs(pc - PC(a64ps_pipe_svc));
            add_obs(uc->uc_mcontext.regs[0]);
        }
        apply_effects(uc, fp);
        x->flag[0] = 1;
        break;
    }
    case K_NEST: {
        if (depth == 1 && sig == c->sig && g.nest_phase == 0 && pc_allowed(c, pc, 0)) {
            struct a64p_fpsimd_rec *fp = check_frame(sig, uc, on_alt_expected);
            check_ctx("nest-outer", uc, fp);
            g.outer_mask = cur_mask();
            add_obs(uc_mask(uc));
            add_obs(g.outer_mask);
            g.nest_phase = 1;
            /* The nested signal must arrive while the handler waits for it. */
            rw_ret_t rr = a64ps_raise_wait(x->pid, x->tid, (uint64_t)c->sig2, &x->flag[2],
                                           4000000000ULL);
            g.nest_phase = 2;
            add_obs(rr.flag);
            add_obs(rr.mark);
            if (rr.flag != 1)
                hfail("nest: the nested signal %d was not delivered during the handler",
                      c->sig2);
            else if (rr.mark != 0x5eedf00dULL) {
                hfail("nest: the nested handler's write of x5 was lost (0x%llx)",
                      (unsigned long long)rr.mark);
            }
            apply_effects(uc, fp);
            x->flag[0] = 1;
        } else if (depth == 2 && sig == c->sig2 && g.nest_phase == 1) {
            /* Interrupting a64ps_raise_wait after its svc. */
            struct a64p_fpsimd_rec *fp = check_frame(sig, uc, -1);
            (void)fp;
            if (pc < PC(a64ps_rw_after) || pc >= PC(a64ps_rw_end) ||
                uc->uc_mcontext.regs[3] != (uint64_t)(uintptr_t)&x->flag[2] ||
                uc->uc_mcontext.regs[2] != (uint64_t)c->sig2 ||
                uc->uc_mcontext.regs[8] != 131) {
                hfail("nest: nested context pc +0x%llx x2 %llu x3 0x%llx x8 %llu",
                      (unsigned long long)(pc - PC(a64ps_code_lo)),
                      (unsigned long long)uc->uc_mcontext.regs[2],
                      (unsigned long long)uc->uc_mcontext.regs[3],
                      (unsigned long long)uc->uc_mcontext.regs[8]);
            } else
                g.ctx_checked++;
            add_obs(uc_mask(uc));
            uc->uc_mcontext.regs[5] = 0x5eedf00dULL;
            x->flag[2] = 1;
        } else
            g.strays++;
        break;
    }
    case K_SAMASK: {
        if (depth == 1 && sig == c->sig && !g.outer_done && pc_allowed(c, pc, 0)) {
            struct a64p_fpsimd_rec *fp = check_frame(sig, uc, on_alt_expected);
            check_ctx("samask-outer", uc, fp);
            add_obs(uc_mask(uc));
            add_obs(cur_mask());
            rw_ret_t rr =
                a64ps_raise_wait(x->pid, x->tid, (uint64_t)c->sig2, &x->flag[2], 300000);
            add_obs(rr.flag);
            if (rr.flag != 0)
                hfail("samask: signal %d was delivered although sa_mask blocks it",
                      c->sig2);
            sigset_t p;
            sigpending(&p);
            add_obs((uint64_t)sigismember(&p, c->sig2));
            if (!sigismember(&p, c->sig2))
                hfail("samask: signal %d is not pending in the handler", c->sig2);
            apply_effects(uc, fp);
            model_apply(c);
            g.outer_done = 1;
        } else if (depth == 1 && sig == c->sig2 && g.outer_done && x->flag[0] == 0) {
            if (!pc_allowed(c, pc, 0))
                hfail("samask: signal %d taken at pc +0x%llx after sigreturn", sig,
                      (unsigned long long)(pc - PC(a64ps_code_lo)));
            struct a64p_fpsimd_rec *fp = check_frame(sig, uc, on_alt_expected);
            /* The outer handler's effects are in this context already. */
            check_ctx("samask-after", uc, fp);
            add_obs(uc_mask(uc));
            x->flag[2] = 1;
            x->flag[0] = 1;
        } else if (sig == c->sig2) {
            hfail("samask: signal %d delivered at depth %d before the handler returned",
                  sig, depth);
            x->flag[2] = 1;
        } else
            g.strays++;
        break;
    }
    case K_ALT: {
        if (sig != c->sig || depth != 1 || x->flag[1] != 0) {
            g.strays++;
            break;
        }
        int phase = x->flag[0] == 0 ? 0 : 1;
        if (!pc_allowed(c, pc, phase)) {
            hfail("alt: signal %d (phase %d) at pc +0x%llx", sig, phase,
                  (unsigned long long)(pc - PC(a64ps_code_lo)));
            x->flag[phase] = 0xdead;
            break;
        }
        if (phase == 1) {
            /* The second signal: after the tgkill, with phase 1's effects. */
            g.mx[0] = 0;
            g.mx[1] = x->tid;
            g.mx[2] = x->sig;
            g.mx[8] = 131;
        }
        struct a64p_fpsimd_rec *fp = check_frame(sig, uc, phase == 0);
        check_ctx(phase == 0 ? "alt-on" : "alt-off", uc, fp);
        int local;
        uint64_t la = (uint64_t)(uintptr_t)&local;
        add_obs(la >= SG_ALT_LO && la < SG_ALT_LO + SG_ALT_SIZE);
        add_obs((uint64_t)(uintptr_t)uc->uc_stack.ss_sp);
        add_obs((uint64_t)uc->uc_stack.ss_size);
        add_obs((uint64_t)uc->uc_stack.ss_flags);
        stack_t q;
        sigaltstack(NULL, &q);
        add_obs((uint64_t)q.ss_flags);
        add_obs((uint64_t)(uintptr_t)q.ss_sp);
        if (phase == 0) {
            apply_effects(uc, fp);
            model_apply(c);
            x->flag[0] = 1;
        } else
            x->flag[1] = 1;
        break;
    }
    case K_RT: {
        if (depth == 1 && (in_code || !rt_nesting)) {
            if (!pc_allowed(c, pc, 0)) {
                hfail("rt: signal %d at pc +0x%llx", sig,
                      (unsigned long long)(pc - PC(a64ps_code_lo)));
            } else {
                struct a64p_fpsimd_rec *fp = check_frame(sig, uc, c->onstack);
                check_ctx("rt", uc, fp);
            }
        } else
            check_frame(sig, uc, -1);
        if (g.r->nlog < NLOG) {
            g.r->log[g.r->nlog++] = (uint64_t)sig << 40 |
                (uint64_t)(uint8_t)si->si_code << 32 | (uint32_t)si->si_value.sival_int;
        }
        if (si->si_pid != getpid())
            hfail("rt: signal %d si_pid %d, not %d", sig, si->si_pid, getpid());
        if (++g.nrt_log >= c->nexpect)
            x->flag[0] = 1;
        break;
    }
    default: g.strays++; break;
    }
out:
    g.depth--;
}

/***************************************************************************
 * Helper thread: external signal sources and the watchdog.
 */
enum { H_NONE, H_TGKILL, H_KILL, H_PIPE };
typedef struct {
    volatile int req, done, quit;
    volatile uint64_t deadline;
    volatile int hung;
    int sig, restart;
    pthread_t thr;
} helper_t;
static helper_t hp;

/* Whether the main thread is blocked in read(fd). */
static int
main_in_read(int fd)
{
    char path[64], buf[256];
    snprintf(path, sizeof(path), "/proc/self/task/%d/syscall", main_tid);
    int f = open(path, O_RDONLY);
    if (f < 0)
        return 0;
    ssize_t n = read(f, buf, sizeof(buf) - 1);
    close(f);
    if (n <= 0)
        return 0;
    buf[n] = '\0';
    unsigned long nr = 0, a0 = 0;
    if (sscanf(buf, "%lu 0x%lx", &nr, &a0) != 2)
        return 0;
    return nr == 63 && a0 == (unsigned long)fd;
}

static void
helper_wake_all(void)
{
    sgctx_t *x = SG_CTX;
    for (int i = 0; i < 4; i++) {
        if (x->flag[i] == 0)
            x->flag[i] = 0xdead;
    }
    if (write(pipe_fd[1], "h", 1) < 0) {
        /* Nothing more we can do. */
    }
}

static void *
helper_main(void *arg)
{
    sigset_t all;
    sigfillset(&all);
    pthread_sigmask(SIG_BLOCK, &all, NULL);
    sgctx_t *x = SG_CTX;
    while (!hp.quit) {
        int req = __atomic_load_n(&hp.req, __ATOMIC_ACQUIRE);
        if (req != H_NONE && !hp.done) {
            int stage = 0;
            uint64_t last = 0;
            while (!hp.hung) {
                if (req == H_PIPE) {
                    if (stage == 0 && main_in_read((int)x->aux)) {
                        syscall(SYS_tgkill, getpid(), main_tid, hp.sig);
                        stage = 1;
                    } else if (stage == 1 && x->flag[0] != 0) {
                        if (hp.restart && write(pipe_fd[1], "p", 1) < 0)
                            break;
                        break;
                    }
                    sched_yield();
                    continue;
                }
                if (x->flag[0] != 0)
                    break;
                if (x->started && stage == 0) {
                    stage = 1;
                    if (req == H_TGKILL)
                        syscall(SYS_tgkill, getpid(), main_tid, hp.sig);
                    else
                        kill(getpid(), hp.sig);
                }
                sched_yield();
            }
            __atomic_store_n(&hp.done, 1, __ATOMIC_RELEASE);
        }
        sched_yield();
    }
    return NULL;
}

static void
helper_start(void)
{
    hp.quit = 0;
    hp.req = H_NONE;
    hp.done = 1;
    /* Created with every signal blocked: it never takes the cases' signals. */
    sigset_t all, old;
    sigfillset(&all);
    pthread_sigmask(SIG_BLOCK, &all, &old);
    int res = pthread_create(&hp.thr, NULL, helper_main, NULL);
    pthread_sigmask(SIG_SETMASK, &old, NULL);
    if (res != 0) {
        fprintf(stderr, "ERROR: cannot create the helper thread\n");
        exit(3);
    }
}

static void
helper_stop(void)
{
    hp.quit = 1;
    pthread_join(hp.thr, NULL);
}

/***************************************************************************
 * Running one case.
 */
static timer_t ptimer;
static int ptimer_ok;

static void
set_handler(int sig, void (*h)(int, siginfo_t *, void *), int flags, uint64_t mask)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = h;
    sa.sa_flags = SA_SIGINFO | flags;
    sigemptyset(&sa.sa_mask);
    for (int i = 1; i <= 64; i++) {
        if (mask & (1ULL << (i - 1)))
            sigaddset(&sa.sa_mask, i);
    }
    if (sigaction(sig, &sa, NULL) != 0) {
        perror("sigaction");
        exit(3);
    }
}

static void
drain_pipe(void)
{
    int n = 0;
    char b[64];
    while (ioctl(pipe_fd[0], FIONREAD, &n) == 0 && n > 0) {
        if (read(pipe_fd[0], b, n > 64 ? 64 : n) <= 0)
            break;
    }
}

static void
setup_ctx(sgcase_t *c)
{
    sgctx_t *x = SG_CTX;
    rng_t r = { c->seed * 0x9e3779b97f4a7c15ULL + 77 + (uint64_t)c->kind };
    memset(x, 0, sizeof(*x));
    for (int i = 0; i < 31; i++)
        x->in_x[i] = rval(&r);
    x->in_x[28] = (uint64_t)(uintptr_t)x;
    for (int i = 0; i < 32; i++)
        x->in_v[i] = ((__uint128_t)rnext(&r) << 64) | rnext(&r);
    x->in_nzcv = (uint64_t)rn(&r, 16) << 28;
    x->in_dit = has_dit && rp(&r, 60) ? PSTATE_DIT : 0;
    x->in_sp = SG_STK_TOP - 0x100 - 16 * rn(&r, 8);
    x->body = a64ps_bodies[kind_body[c->kind]];
    x->has_dit = has_dit;
    x->pid = (uint64_t)getpid();
    x->tid = (uint64_t)main_tid;
    x->sig = (uint64_t)c->sig;
    x->aux = (uint64_t)pipe_fd[0];
    x->aux2 = SG_GUARD;
    x->tgt[0] = PC(a64ps_tgt0);
    x->tgt[1] = PC(a64ps_tgt1);
    x->tgt[2] = PC(a64ps_tgt2);
    x->tgt[3] = PC(a64ps_tgt3);
    x->set = 1ULL << (c->sig - 1);
    if (c->kind == K_RT) {
        x->set = 0;
        x->count = (uint64_t)c->nrt;
        for (int i = 0; i < c->nrt; i++) {
            siginfo_t *si = (siginfo_t *)x->rttab[i];
            memset(si, 0, 128);
            si->si_signo = c->rt_sig[i];
            si->si_code = SI_QUEUE;
            si->si_pid = getpid();
            si->si_uid = getuid();
            si->si_value.sival_int = c->rt_val[i];
            x->set |= 1ULL << (c->rt_sig[i] - 1);
        }
    }
    x->ss_on.ss_sp = (void *)SG_ALT_LO;
    x->ss_on.ss_size = SG_ALT_SIZE - 16 * rn(&r, 64);
    x->ss_on.ss_flags = 0;
    x->ss_off.ss_flags = SS_DISABLE;
}

/* Runs case c, filling res.  Returns the number of property failures. */
static int
run_case(sgcase_t *c, sgres_t *res)
{
    sgctx_t *x = SG_CTX;
    memset(res, 0, sizeof(*res));
    setup_ctx(c);
    memset(&g.mx, 0, sizeof(g) - offsetof(sgrun_t, mx));
    g.c = c;
    g.r = res;
    __asm__ volatile("mrs %0, fpcr" : "=r"(g.fpcr));
    model_first(c, x);
    g.native_za_vl =
        (getauxval(AT_HWCAP2) & HWCAP2_SME) ? (long)(prctl(PR_SME_GET_VL) & 0xffff) : -1;

    /* Handlers. */
    int flags = SA_RESTART;
    if (c->kind == K_PIPE)
        flags = c->restart ? SA_RESTART : 0;
    if (c->onstack || c->kind == K_ALT)
        flags |= SA_ONSTACK;
    if (c->kind == K_UDF || c->kind == K_BRK || c->kind == K_SEGV) {
        set_handler(c->kind == K_UDF       ? SIGILL
                        : c->kind == K_BRK ? SIGTRAP
                                           : SIGSEGV,
                    h_sync, flags | (c->nodefer ? SA_NODEFER : 0), c->samask);
    } else if (c->kind == K_NEST) {
        set_handler(c->sig, h_async, flags | (c->nodefer ? SA_NODEFER : 0), c->samask);
        if (c->sig2 != c->sig)
            set_handler(c->sig2, h_async, flags, c->samask);
    } else if (c->kind == K_SAMASK) {
        set_handler(c->sig, h_async, flags, c->samask | 1ULL << (c->sig2 - 1));
        set_handler(c->sig2, h_async, flags, c->samask);
    } else if (c->kind == K_RT) {
        uint64_t all = SG_CTX->set;
        for (int i = 0; i < c->nrt; i++)
            set_handler(c->rt_sig[i], h_async, flags, c->rt_masked ? all : 0);
    } else
        set_handler(c->sig, h_async, flags, c->samask);

    /* Alternate stack before the run. */
    stack_t ss;
    if (c->onstack && c->kind != K_ALT) {
        ss.ss_sp = (void *)SG_ALT_LO;
        ss.ss_size = SG_ALT_SIZE;
        ss.ss_flags = 0;
    } else {
        ss.ss_sp = NULL;
        ss.ss_size = 0;
        ss.ss_flags = SS_DISABLE;
    }
    if (sigaltstack(&ss, NULL) != 0) {
        perror("sigaltstack");
        exit(3);
    }
    sigset_t none;
    sigemptyset(&none);
    pthread_sigmask(SIG_SETMASK, &none, NULL);
    drain_pipe();

    /* Sources. */
    hp.deadline = now_ns() + 4000000000ULL;
    hp.hung = 0;
    hp.sig = c->sig;
    hp.restart = c->restart;
    if (c->src == SRC_ITIMER) {
        struct itimerval it = { { 0, 400 }, { 0, 400 } };
        setitimer(ITIMER_REAL, &it, NULL);
    } else if (c->src == SRC_PTIMER) {
        struct itimerspec its = { { 0, 300000 }, { 0, 300000 } };
        if (!ptimer_ok || timer_settime(ptimer, 0, &its, NULL) != 0) {
            fprintf(stderr, "ERROR: POSIX timer unavailable\n");
            exit(3);
        }
    }
    int req = c->kind == K_PIPE ? H_PIPE
        : c->src == SRC_TGKILL  ? H_TGKILL
        : c->src == SRC_KILL    ? H_KILL
                                : H_NONE;
    hp.done = 0;
    __atomic_store_n(&hp.req, req, __ATOMIC_RELEASE);

    g.running = 1;
    a64ps_run(x);
    g.running = 0;

    if (c->src == SRC_ITIMER) {
        struct itimerval it;
        memset(&it, 0, sizeof(it));
        setitimer(ITIMER_REAL, &it, NULL);
    } else if (c->src == SRC_PTIMER) {
        struct itimerspec its;
        memset(&its, 0, sizeof(its));
        timer_settime(ptimer, 0, &its, NULL);
    }
    if (req != H_NONE) {
        while (!__atomic_load_n(&hp.done, __ATOMIC_ACQUIRE))
            sched_yield();
    }
    __atomic_store_n(&hp.req, H_NONE, __ATOMIC_RELEASE);
    ss.ss_flags = SS_DISABLE;
    sigaltstack(&ss, NULL);

    /* Results. */
    int fails = g.fails;
    char why[640];
    snprintf(why, sizeof(why), "%s", g.fail_msg);
    if (hp.hung) {
        fails++;
        snprintf(
            why, sizeof(why),
            "HANG: the case did not finish within 4 s (flags %llx %llx %llx, started "
            "%llu, result %lld)%s%s",
            (unsigned long long)x->flag[0], (unsigned long long)x->flag[1],
            (unsigned long long)x->flag[2], (unsigned long long)x->started,
            (long long)x->result, g.fails ? "; first: " : "", g.fail_msg);
    }
    res->valid = 1;
    res->conclusive = 1;
    memcpy(res->out_x, x->out_x, sizeof(res->out_x));
    res->out_sp = x->out_sp;
    res->out_nzcv = x->out_nzcv & PSTATE_NZCV;
    res->out_dit = x->out_dit;
    res->out_cont = x->out_cont;
    memcpy(res->out_v, x->out_v, sizeof(res->out_v));
    add_obs((uint64_t)x->result);
    add_obs(x->pend);
    if (c->kind == K_ALT) {
        add_obs((uint64_t)(uintptr_t)x->ss_q.ss_sp);
        add_obs((uint64_t)x->ss_q.ss_flags);
        add_obs((uint64_t)x->ss_q.ss_size);
    }

    /* The model of the final state. */
    if (!hp.hung) {
        char buf[256];
        int bad = 0;
        if (c->kind != K_SAMASK && c->kind != K_ALT && kind_effects_once(c))
            model_apply(c);
        if (c->kind == K_ALT) {
            g.mx[0] = 0;
            g.mx[1] = (uint64_t)(uintptr_t)x + O_SS_Q;
            g.mx[2] = x->sig;
            g.mx[8] = 132;
        } else if (c->kind == K_PIPE)
            g.mx[0] = c->restart ? 1 : (uint64_t)-EINTR;
        uint64_t cont = (c->eff & EFF_PC) ? (uint64_t)c->cont : 0;
        buf[0] = '\0';
        for (int i = 0; i < 31; i++) {
            if (i == 16 || (i == 30 && c->kind == K_ISPIN))
                continue;
            uint64_t want = i == 17 ? cont : g.mx[i];
            if (res->out_x[i] != want && bad++ == 0) {
                snprintf(buf, sizeof(buf), "x%d 0x%llx, model 0x%llx", i,
                         (unsigned long long)res->out_x[i], (unsigned long long)want);
            }
        }
        if (res->out_sp != g.msp && bad++ == 0) {
            snprintf(buf, sizeof(buf), "sp 0x%llx, model 0x%llx",
                     (unsigned long long)res->out_sp, (unsigned long long)g.msp);
        }
        if (res->out_nzcv != g.mnzcv && bad++ == 0) {
            snprintf(buf, sizeof(buf), "nzcv 0x%llx, model 0x%llx",
                     (unsigned long long)res->out_nzcv, (unsigned long long)g.mnzcv);
        }
        if (has_dit && res->out_dit != g.mdit && bad++ == 0) {
            snprintf(buf, sizeof(buf), "DIT 0x%llx, model 0x%llx",
                     (unsigned long long)res->out_dit, (unsigned long long)g.mdit);
        }
        if (res->out_cont != cont && bad++ == 0) {
            snprintf(buf, sizeof(buf), "continuation %llu, model %llu",
                     (unsigned long long)res->out_cont, (unsigned long long)cont);
        }
        for (int i = 0; i < 32; i++) {
            if (res->out_v[i] != g.mv[i] && bad++ == 0)
                snprintf(buf, sizeof(buf), "v%d differs from the model", i);
        }
        /* Live stack above the final SP: the canaries. */
        uint64_t lo = x->in_sp - SG_CANARY;
        if (g.msp > lo)
            lo = g.msp;
        for (uint64_t a = lo; a < x->in_sp; a += 8) {
            uint64_t want = x->in_x[(a - (x->in_sp - SG_CANARY)) / 8];
            if (*(uint64_t *)a != want && bad++ == 0) {
                snprintf(buf, sizeof(buf),
                         "live stack word at 0x%llx: 0x%llx, canary 0x%llx",
                         (unsigned long long)a, (unsigned long long)*(uint64_t *)a,
                         (unsigned long long)want);
            }
        }
        /* Scenario-specific expectations. */
        if (c->kind == K_MASK) {
            if (x->result != 0 && bad++ == 0)
                snprintf(buf, sizeof(buf), "handler ran %lld times while blocked",
                         (long long)x->result);
            if (!(x->pend & (1ULL << (c->sig - 1))) && bad++ == 0)
                snprintf(buf, sizeof(buf), "blocked signal not pending (0x%llx)",
                         (unsigned long long)x->pend);
        } else if (c->kind == K_RT) {
            if (x->pend != x->set && bad++ == 0)
                snprintf(buf, sizeof(buf), "pending 0x%llx, queued 0x%llx",
                         (unsigned long long)x->pend, (unsigned long long)x->set);
            if (res->nlog != c->nexpect && bad++ == 0)
                snprintf(buf, sizeof(buf), "%d deliveries, expected %d", res->nlog,
                         c->nexpect);
            /* Each payload once (strict); per signal in queue order and, with
             * the handlers masking each other, lowest signal first (the order;
             * see the rt-overflow-order known deviation).
             */
            int used[RT_MAX];
            memset(used, 0, sizeof(used));
            int prev_sig = 0, order_bad = 0;
            char obuf[128];
            obuf[0] = '\0';
            for (int k = 0; k < res->nlog; k++) {
                int s = (int)(res->log[k] >> 40);
                uint32_t v = (uint32_t)res->log[k];
                int found = -1, first = -1;
                for (int i = 0; i < c->nrt; i++) {
                    if (c->rt_sig[i] != s || used[i])
                        continue;
                    if (first < 0)
                        first = i;
                    if (found < 0 && (s < SIGRTMIN || (uint32_t)c->rt_val[i] == v))
                        found = i;
                }
                if (found < 0) {
                    if (bad++ == 0)
                        snprintf(buf, sizeof(buf), "delivery %d: signal %d value %u", k,
                                 s, v);
                    break;
                }
                if (found != first && order_bad++ == 0) {
                    snprintf(obuf, sizeof(obuf),
                             "delivery %d: signal %d value %u ahead of earlier %u", k, s,
                             v, (unsigned)c->rt_val[first]);
                }
                used[found] = 1;
                if (s < SIGRTMIN) {
                    /* Coalesced: consume every queued instance. */
                    for (int i = 0; i < c->nrt; i++) {
                        if (c->rt_sig[i] == s)
                            used[i] = 1;
                    }
                }
                if (c->rt_masked && s < prev_sig && order_bad++ == 0) {
                    snprintf(obuf, sizeof(obuf), "signal %d delivered after %d", s,
                             prev_sig);
                }
                prev_sig = s;
            }
            if (order_bad > 0) {
                if (rt_overflow_deviation(c)) {
                    model_stat[ST_KD_RT_ORDER]++;
                    if (verbose)
                        fprintf(stderr,
                                "  case %d: known deviation rt-overflow-order: %s\n",
                                c->idx, obuf);
                } else if (bad++ == 0)
                    snprintf(buf, sizeof(buf), "%s", obuf);
            }
        } else if (c->kind == K_PIPE) {
            int64_t want = c->restart ? 1 : -EINTR;
            if (x->result != want && bad++ == 0)
                snprintf(buf, sizeof(buf), "read returned %lld, expected %lld",
                         (long long)x->result, (long long)want);
        } else if (c->kind == K_ALT) {
            if (x->result != 0 && bad++ == 0)
                snprintf(buf, sizeof(buf), "disabling the altstack returned %lld",
                         (long long)x->result);
            if (x->ss_q.ss_flags != SS_DISABLE && bad++ == 0)
                snprintf(buf, sizeof(buf), "final sigaltstack flags %d",
                         x->ss_q.ss_flags);
        }
        if (g.eff_applied == 0 && kind_effects_once(c) && bad++ == 0)
            snprintf(buf, sizeof(buf), "the handler never applied its effects");
        if (bad > 0) {
            fails++;
            if (why[0] == '\0') {
                snprintf(why, sizeof(why),
                         "final state differs from the model in %d fields; "
                         "first: %s",
                         bad, buf);
            }
        }
    }
    if (fails > 0) {
        fprintf(stderr, "  case %d (%s, seed %llu): %s\n", c->idx, kind_name[c->kind],
                (unsigned long long)c->seed, why);
    }
    /* The native and DR runs are different processes: compare the thread id a
     * register holds (after a tgkill) as a token.
     */
    for (int i = 0; i < 31; i++) {
        if (res->out_x[i] == x->tid)
            res->out_x[i] = 0x7d7d7d7dULL;
    }
    model_stat[ST_CTX_CHECKED] += g.ctx_checked;
    model_stat[ST_EFF_APPLIED] += g.eff_applied;
    model_stat[ST_FRAMES_OK] += g.frames_ok;
    model_stat[ST_STRAYS] += g.strays + g.other_thread;
    g.c = NULL;
    return fails;
}

static void
print_case(FILE *o, const sgcase_t *c)
{
    fprintf(
        o,
        "    case %d kind %s seed %llu sig %d sig2 %d src %s restart %d nodefer %d "
        "onstack %d samask 0x%llx eff 0x%x gpr x%d=0x%llx nzcv 0x%llx dit 0x%llx cont "
        "%d sp%+lld v%d nrt %d masked %d\n",
        c->idx, kind_name[c->kind], (unsigned long long)c->seed, c->sig, c->sig2,
        src_name[c->src], c->restart, c->nodefer, c->onstack,
        (unsigned long long)c->samask, c->eff, c->gpr, (unsigned long long)c->gval,
        (unsigned long long)c->nzcv, (unsigned long long)c->dit, c->cont,
        (long long)c->spdelta, c->vreg, c->nrt, c->rt_masked);
    if (c->kind == K_RT) {
        fprintf(o, "      queued:");
        for (int i = 0; i < c->nrt; i++)
            fprintf(o, " %d:%u", c->rt_sig[i], (unsigned)c->rt_val[i]);
        fprintf(o, "\n");
    }
}

static void
print_log(FILE *o, const char *who, const sgres_t *r)
{
    fprintf(o, "      %s deliveries:", who);
    for (int i = 0; i < r->nlog; i++)
        fprintf(o, " %d:%u", (int)(r->log[i] >> 40), (unsigned)(uint32_t)r->log[i]);
    fprintf(o, "\n");
}

/* Native vs DR: everything the scenario determines. */
static int
cmp_res(const sgcase_t *c, const sgres_t *n, const sgres_t *d)
{
    int bad = 0;
    char buf[256];
    buf[0] = '\0';
    for (int i = 0; i < 31; i++) {
        if (i == 16 || (c->kind == K_ISPIN && i == 30))
            continue;
        if (n->out_x[i] != d->out_x[i] && bad++ == 0)
            snprintf(buf, sizeof(buf), "x%d native 0x%llx dr 0x%llx", i,
                     (unsigned long long)n->out_x[i], (unsigned long long)d->out_x[i]);
    }
#define RC(f)                                                             \
    do {                                                                  \
        if (n->f != d->f && bad++ == 0)                                   \
            snprintf(buf, sizeof(buf), #f " native 0x%llx dr 0x%llx",     \
                     (unsigned long long)n->f, (unsigned long long)d->f); \
    } while (0)
    RC(out_sp);
    RC(out_nzcv);
    RC(out_dit);
    RC(out_cont);
    RC(layout);
    RC(nobs);
    RC(nlog);
#undef RC
    if (memcmp(n->out_v, d->out_v, sizeof(n->out_v)) != 0 && bad++ == 0)
        snprintf(buf, sizeof(buf), "V registers differ");
    for (int i = 0; i < n->nobs && i < d->nobs; i++) {
        if (n->obs[i] != d->obs[i] && bad++ == 0)
            snprintf(buf, sizeof(buf), "observation %d native 0x%llx dr 0x%llx", i,
                     (unsigned long long)n->obs[i], (unsigned long long)d->obs[i]);
    }
    if (c->kind == K_RT) {
        /* With nesting allowed, only per-signal order is determined (checked
         * against the model); with masking handlers the whole order is.
         */
        for (int i = 0;
             c->rt_masked && !rt_overflow_deviation(c) && i < n->nlog && i < d->nlog;
             i++) {
            if (n->log[i] != d->log[i] && bad++ == 0)
                snprintf(buf, sizeof(buf), "delivery %d native 0x%llx dr 0x%llx", i,
                         (unsigned long long)n->log[i], (unsigned long long)d->log[i]);
        }
    }
    if (bad > 0) {
        fprintf(stderr,
                "  case %d (%s): DR differs from native in %d fields; first: %s\n",
                c->idx, kind_name[c->kind], bad, buf);
    }
    if (c->kind == K_RT && (bad > 0 || verbose)) {
        print_log(stderr, "native", n);
        print_log(stderr, "dr", d);
    }
    return bad;
}

static void
child_setup(void)
{
    main_tid = (pid_t)syscall(SYS_gettid);
    if (pipe(pipe_fd) != 0) {
        perror("pipe");
        exit(3);
    }
    struct sigevent sev;
    memset(&sev, 0, sizeof(sev));
    sev.sigev_notify = SIGEV_THREAD_ID;
    sev.sigev_signo = SIGRTMIN + 9;
    sev._sigev_un._tid = main_tid;
    ptimer_ok = timer_create(CLOCK_MONOTONIC, &sev, &ptimer) == 0;
    /* Every signal a case may use has a handler, so a late one is harmless. */
    for (int i = 0; i < 8; i++)
        set_handler(sig_pool[i], h_async, SA_RESTART, 0);
    set_handler(SIGALRM, h_async, SA_RESTART, 0);
    set_handler(SIGRTMIN + 9, h_async, SA_RESTART, 0);
    for (int i = 0; i < 8; i++)
        set_handler(SIGRTMIN + 3 + i, h_async, SA_RESTART, 0);
    helper_start();
}

static void
child_teardown(void)
{
    helper_stop();
    if (ptimer_ok)
        timer_delete(ptimer);
    close(pipe_fd[0]);
    close(pipe_fd[1]);
}


/* Only called from main after run_case returns, never from a handler. The
 * snapshot contains raw fixture register bits, but omits structural addresses
 * and padding. SVE payload-present reports the actual full record extent, not
 * an inference from the layout hash or proof the app initialized that state.
 * Streaming-mode FFR interpretation additionally depends on SME_FA64 support.
 * At most 12 headers and 9248 register bytes produce less than 24 KiB of text
 * once per UDF process. No optional-record payload besides FPSIMD/SVE is dumped.
 */
static void
dump_udf_bytes(const char *name, int reg, const uint8_t *data, size_t size)
{
    static const char digits[] = "0123456789abcdef";
    char text[2 * A64P_SNAPSHOT_VL + 1];
    for (size_t i = 0; i < size; i++) {
        text[2 * i] = digits[data[i] >> 4];
        text[2 * i + 1] = digits[data[i] & 15];
    }
    text[2 * size] = '\0';
    if (reg < 0)
        fprintf(stderr, "udf-frame: %s=%s\n", name, text);
    else
        fprintf(stderr, "udf-frame: %s%d=%s\n", name, reg, text);
}

static const char *
udf_record_name(uint32_t magic)
{
    switch (magic) {
    case 0: return "END";
    case FPSIMD_MAGIC_: return "FPSIMD";
    case ESR_MAGIC_: return "ESR";
    case EXTRA_MAGIC_: return "EXTRA";
    case SVE_MAGIC_: return "SVE";
    case ZA_MAGIC_: return "ZA";
    case ZT_MAGIC_: return "ZT";
    case TPIDR2_MAGIC_: return "TPIDR2";
    case FPMR_MAGIC_: return "FPMR";
    case POE_MAGIC_: return "POE";
    case GCS_MAGIC_: return "GCS";
    default: return "UNKNOWN";
    }
}

static void
dump_first_udf_frame(void)
{
    const struct a64p_frame_snapshot *s = &first_udf_frame;
    fprintf(stderr, "udf-frame: attempted=%d valid=%d case=%d seed=%llu "
            "stage=before-effects headers=%u truncated=%d byte-order=memory\n",
            s->attempted, s->valid, first_udf_index,
            (unsigned long long)first_udf_seed, s->nrecords, s->metadata_truncated);
    if (s->attempted)
        fprintf(stderr, "udf-frame: frame-on-alt=%d expected-alt=%d position=%s-sp distance=%llu\n",
                first_udf_on_alt, first_udf_expected_alt,
                first_udf_below_sp ? "below" : "above",
                (unsigned long long)first_udf_sp_distance);
    for (unsigned i = 0; i < s->nrecords; i++) {
        fprintf(stderr, "udf-frame: record=%u region=%s offset=%zu kind=%s "
                "magic=%08x size=%u\n", i, s->records[i].extra ? "extra" : "reserved",
                s->records[i].offset, udf_record_name(s->records[i].magic),
                s->records[i].magic, s->records[i].size);
    }
    if (s->extra_present)
        fprintf(stderr, "udf-frame: extra-bytes=%u position=%s-reserved-end distance=%zu\n",
                s->extra_size, s->extra_after_reserved ? "after" : "before",
                s->extra_distance);
    if (s->fpsimd_present) {
        fprintf(stderr, "udf-frame: fpcr=%08x fpsr=%08x\n", s->fpcr, s->fpsr);
        for (int i = 0; i < 32; i++)
            dump_udf_bytes("v", i, s->v + 16 * i, 16);
    }
    fprintf(stderr, "udf-frame: sve-present=%d vl-bytes=%u flags=%04x payload-present=%d\n",
            s->sve_present, s->sve_vl, s->sve_flags, s->sve_payload_present);
    if (s->sve_payload_present) {
        for (int i = 0; i < 32; i++)
            dump_udf_bytes("z", i, s->z + s->sve_vl * i, s->sve_vl);
        for (int i = 0; i < 16; i++)
            dump_udf_bytes("p", i, s->p + s->sve_vl / 8 * i, s->sve_vl / 8);
        dump_udf_bytes("ffr", -1, s->ffr, s->sve_vl / 8);
    }
}

/* CLI: unix-signal_model abi|paths KIND. Exact original seed/index and per-kind
 * populations (600 ABI, 150 paths). Model failures are fatal even natively;
 * no inconclusive-case floor or source RT-order suppression is imported.
 * stdout serializes every field compared by the original cmp_res, avoiding
 * padding or process-specific TIDs. Each profile is a genuinely fresh image. */
int main(int argc,char **argv) {
    if(argc!=3)return 2;
    int paths=!strcmp(argv[1],"paths"),nk=paths?K_NUM:K_ISPIN,kind=-1;
    if(!paths && strcmp(argv[1],"abi"))return 2;
    for(int i=0;i<nk;i++)if(!strcmp(argv[2],kind_name[i]))kind=i;
    if(kind<0)return 2;
    has_dit=(getauxval(AT_HWCAP)&HWCAP_DIT)!=0;
    int pool[8]={SIGUSR1,SIGUSR2,SIGRTMIN+3,SIGRTMIN+4,SIGRTMIN+5,SIGRTMIN+6,SIGRTMIN+7,SIGRTMIN+8};
    memcpy(sig_pool,pool,sizeof pool);
    void *area=mmap((void *)SG_BASE,SG_MAP_SIZE,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS|MAP_FIXED_NOREPLACE,-1,0);
    if(area!=(void *)SG_BASE || mprotect((void *)SG_GUARD,0x1000,PROT_NONE))return 2;
    child_setup();int count=paths?150:600;
    for(int i=0;i<count;i++) {
        int idx=i*nk+kind;sgcase_t c;sgres_t r;
        gen_case(kind,idx,1+(uint64_t)idx,&c,paths);
        int failed=run_case(&c,&r);
        if(i==0 && kind==K_UDF)dump_first_udf_frame();
        if(failed) {print_case(stderr,&c);return 1;}
        printf("%d",idx);
        for(int j=0;j<31;j++)if(j!=16 && !(kind==K_ISPIN && j==30))printf(" %llx",(unsigned long long)r.out_x[j]);
        printf(" %llx %llx %llx %llx %llx %d %d",(unsigned long long)r.out_sp,(unsigned long long)r.out_nzcv,(unsigned long long)r.out_dit,(unsigned long long)r.out_cont,(unsigned long long)r.layout,r.nobs,r.nlog);
        for(int j=0;j<32;j++)printf(" %016llx%016llx",(unsigned long long)(r.out_v[j]>>64),(unsigned long long)r.out_v[j]);
        for(int j=0;j<r.nobs;j++)printf(" %llx",(unsigned long long)r.obs[j]);
        if(kind==K_RT && c.rt_masked)for(int j=0;j<r.nlog;j++)printf(" %llx",(unsigned long long)r.log[j]);
        putchar('\n');
    }
    child_teardown();
    if(!model_stat[ST_CTX_CHECKED] || !model_stat[ST_FRAMES_OK] || model_stat[ST_STRAYS])return 1;
    fprintf(stderr,"model: ctx %ld frames %ld effects %ld DIT %d\n",model_stat[ST_CTX_CHECKED],model_stat[ST_FRAMES_OK],model_stat[ST_EFF_APPLIED],has_dit);
    printf("OK %s %s %d\n",argv[1],argv[2],count);
    return 0;
}
