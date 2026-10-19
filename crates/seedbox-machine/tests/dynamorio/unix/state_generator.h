/* Source 356c1821045a4b5678d0813e132074324623df6e a64_dr_prop_gen.h; original bounded lifecycle block generator. */
/* **********************************************************
 * Copyright (c) 2026 Google, Inc.  All rights reserved.
 * **********************************************************/

/*
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * * Redistributions of source code must retain the above copyright notice,
 *   this list of conditions and the following disclaimer.
 *
 * * Redistributions in binary form must reproduce the above copyright notice,
 *   this list of conditions and the following disclaimer in the documentation
 *   and/or other materials provided with the distribution.
 *
 * * Neither the name of Google, Inc. nor the names of its contributors may be
 *   used to endorse or promote products derived from this software without
 *   specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL GOOGLE, INC. OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
 * DAMAGE.
 */

/* Random A64 code block generator for a64_dr_prop.c.
 *
 * The generator emits raw A64 encodings (it deliberately does not use DR's
 * encoder: the code runs natively as the oracle and under DR as the subject).
 * Every block is valid, deterministic, terminating code: it tracks which
 * registers hold known values so that every memory access lands in the data
 * buffer, the initialized part of the stack above SP, or (for planned faults)
 * the guard page.  Each instruction also gets a text description so that a
 * failure can print a readable reproducer.
 */

#ifndef A64_DR_PROP_GEN_H
#define A64_DR_PROP_GEN_H

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <string.h>

/* Not every includer uses every helper. */

/* Fixed addresses so that native and DR runs (in different processes) see
 * identical PC-relative values and pointers.  Each worker that runs generated
 * code owns a layout slot: slot 0 is the original single-worker layout, slot k
 * has its code and data regions displaced by k * A64G_SLOT_STRIDE, so that
 * several workers in one process can generate, install and run blocks at
 * once.  The generator and the support module use the calling thread's slot
 * (a64g_code_base/a64g_data_base: thread-local, slot 0 on every new thread,
 * changed by a64p_worker_bind()), so a block must be generated, installed and
 * run on threads bound to the same slot.
 */
#define A64G_CODE_BASE0 0x3100000000ULL
#define A64G_DATA_BASE0 0x3101000000ULL
#define A64G_SLOT_STRIDE 0x4000000ULL
#define A64G_MAX_SLOTS 64
/* Defined in a64_prop_support.c. */
extern __thread uint64_t a64g_code_base, a64g_data_base;
static inline uint64_t
a64g_slot_code_base(int slot)
{
    return A64G_CODE_BASE0 + (uint64_t)slot * A64G_SLOT_STRIDE;
}
static inline uint64_t
a64g_slot_data_base(int slot)
{
    return A64G_DATA_BASE0 + (uint64_t)slot * A64G_SLOT_STRIDE;
}
#define CODE_BASE a64g_code_base
#define CODE_SIZE 0x10000ULL
#define DATA_BASE a64g_data_base
#define D_ADDR DATA_BASE
#define D_SIZE 0x1000ULL
#define GUARD_ADDR (DATA_BASE + 0x1000ULL)
#define GUARD_SIZE 0x1000ULL
#define STK_LO (DATA_BASE + 0x2000ULL)
#define STK_TOP (DATA_BASE + 0x12000ULL)
/* Only the top STK_CMP bytes of the stack are ever used by blocks: SP never goes
 * below STK_CMP_LO + SP_FLOOR_PAD, leaving ~60K below for signal frames.
 */
#define STK_CMP 0x1000ULL
#define STK_CMP_LO (STK_TOP - STK_CMP)
#define SP_FLOOR (STK_CMP_LO + 0x200ULL)
#define HARN_ADDR (DATA_BASE + 0x20000ULL)
#define DATA_MAP_SIZE 0x21000ULL

#define U(x) ((uint32_t)(x))

/***************************************************************************
 * PRNG
 */
typedef struct {
    uint64_t s;
} rng_t;

static inline uint64_t
rnext(rng_t *r)
{
    uint64_t z = (r->s += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static inline uint32_t
rn(rng_t *r, uint32_t n)
{
    return n == 0 ? 0 : (uint32_t)(rnext(r) % n);
}

static inline int
rp(rng_t *r, int pct)
{
    return (int)rn(r, 100) < pct;
}

/* Interesting 64-bit values. */
static uint64_t
rval(rng_t *r)
{
    switch (rn(r, 10)) {
    case 0: return 0;
    case 1: return ~0ULL;
    case 2: return 1ULL << rn(r, 64);
    case 3: return rn(r, 256);
    case 4: return (uint64_t)-(int64_t)rn(r, 256);
    case 5: return 0x8000000000000000ULL ^ rn(r, 4);
    case 6: return 0x7fffffffULL + rn(r, 3) - 1;
    case 7: return rnext(r) & 0xffffffffULL;
    default: return rnext(r);
    }
}

/***************************************************************************
 * CPU features (from HWCAP), filled in by the harness.
 */
/* Defined in a64_prop_support.c (a64p_init_features()). */
extern int gen_dczva_ok;

typedef struct {
    int lse, pauth, pacg, dit, fp16, jscvt, fcma, lrcpc, dotprod, rdm, flagm, flagm2,
        frint, i8mm, bf16, aes, crc32, sb, afp, sha2;
} feat_t;
extern feat_t feat;

/***************************************************************************
 * Assembler buffer with labels and per-instruction text.
 */
#define MAXW 6144
#define MAXLAB 1024
#define TXTLEN 64

enum { FX_B26, FX_B19, FX_B14, FX_ADR };

typedef struct {
    uint8_t valid;
    uint8_t base; /* 31 == SP */
    int8_t idx;   /* -1 none, 31 == xzr */
    uint8_t ext;  /* 2 UXTW, 3 LSL, 6 SXTW, 7 SXTX */
    uint8_t shift;
    uint8_t wb;   /* 0 offset, 1 pre, 2 post */
    uint8_t pac;  /* base is a DA/DB-signed pointer (LDRAA/LDRAB) */
    uint8_t must; /* DR must expose a memory operand */
    uint8_t lit;  /* literal: address in litaddr */
    uint8_t zva;  /* DC ZVA */
    uint8_t acc;  /* architectural access kind (MA_*), for access models */
    uint16_t size; /* bytes accessed in each direction (0 for MA_PRFM/flushes) */
    int32_t off;
    uint64_t litaddr;
} meminfo_t;

/* meminfo_t.acc: what the instruction does to memory at the modelled address,
 * architecturally, when it does not fault.  MA_RW is an atomic read-modify-write
 * (LSE, CAS/CASP: the read always happens; whether CAS writes on a failed
 * compare is not modelled here).  A store-exclusive is MA_W (attempted).
 */
enum { MA_NONE, MA_R, MA_W, MA_RW, MA_PRFM, MA_ZVA, MA_DFLUSH, MA_IFLUSH };

typedef struct {
    uint32_t w[MAXW];
    char txt[MAXW][TXTLEN];
    int n;
    int lab[MAXLAB];
    int nlab;
    struct {
        int at, lab, kind;
    } fix[MAXW];
    int nfix;
    meminfo_t mi[MAXW];
    uint8_t isdata[MAXW];
    uint8_t fault_ok[MAXW]; /* a fault at this instruction is planned */
    /* For an exclusive load starting a retry loop: the index after the loop,
     * which a single-stepping tracer runs to (stepping clears the monitor).
     */
    int16_t excl_skip[MAXW];
    int overflow;
} asm_t;

static int
lab_new(asm_t *a)
{
    if (a->nlab >= MAXLAB) {
        a->overflow = 1;
        return 0;
    }
    a->lab[a->nlab] = -1;
    return a->nlab++;
}

static void
lab_bind(asm_t *a, int l)
{
    a->lab[l] = a->n;
}

static int
emit(asm_t *a, uint32_t w, const char *fmt, ...)
{
    if (a->n >= MAXW - 1) {
        a->overflow = 1;
        return a->n - 1;
    }
    va_list ap;
    a->w[a->n] = w;
    va_start(ap, fmt);
    vsnprintf(a->txt[a->n], TXTLEN, fmt, ap);
    va_end(ap);
    memset(&a->mi[a->n], 0, sizeof(a->mi[a->n]));
    a->isdata[a->n] = 0;
    a->fault_ok[a->n] = 0;
    a->excl_skip[a->n] = 0;
    return a->n++;
}

static void
fixup(asm_t *a, int at, int l, int kind)
{
    a->fix[a->nfix].at = at;
    a->fix[a->nfix].lab = l;
    a->fix[a->nfix].kind = kind;
    a->nfix++;
}

static int
resolve(asm_t *a)
{
    for (int i = 0; i < a->nfix; i++) {
        int at = a->fix[i].at;
        int tgt = a->lab[a->fix[i].lab];
        if (tgt < 0)
            return 0;
        int64_t d = (int64_t)tgt - at; /* in instructions */
        switch (a->fix[i].kind) {
        case FX_B26: a->w[at] |= U(d) & 0x3ffffff; break;
        case FX_B19: a->w[at] |= (U(d) & 0x7ffff) << 5; break;
        case FX_B14: a->w[at] |= (U(d) & 0x3fff) << 5; break;
        case FX_ADR: {
            uint32_t imm = U(d * 4);
            a->w[at] |= ((imm & 3) << 29) | (((imm >> 2) & 0x7ffff) << 5);
            break;
        }
        }
    }
    return 1;
}

static inline uint64_t
pc_of(int idx)
{
    return CODE_BASE + 4ULL * idx;
}

/***************************************************************************
 * Register names.
 */
static const char *
rname(int r, int sf, int sp)
{
    static __thread char buf[16][8];
    static __thread int i;
    char *b = buf[i++ & 15];
    if (r == 31)
        snprintf(b, 8, "%s", sp ? (sf ? "sp" : "wsp") : (sf ? "xzr" : "wzr"));
    else
        snprintf(b, 8, "%c%d", sf ? 'x' : 'w', r);
    return b;
}
#define XR(r) rname(r, 1, 0)
#define XS(r) rname(r, 1, 1)
#define WR(r) rname(r, 0, 0)
#define GR(sf, r) rname(r, sf, 0)
#define GS(sf, r) rname(r, sf, 1)

static const char *cond_name[16] = { "eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc",
                                     "hi", "ls", "ge", "lt", "gt", "le", "al", "nv" };

/***************************************************************************
 * Encoders.
 */
static uint32_t
e_addsub_imm(int sf, int op, int S, int sh, uint32_t imm12, int rn, int rd)
{
    return U(sf) << 31 | U(op) << 30 | U(S) << 29 | 0x22u << 23 | U(sh) << 22 |
        (imm12 & 0xfff) << 10 | U(rn) << 5 | U(rd);
}
static uint32_t
e_logic_imm(int sf, int opc, int N, int immr, int imms, int rn, int rd)
{
    return U(sf) << 31 | U(opc) << 29 | 0x24u << 23 | U(N) << 22 | U(immr) << 16 |
        U(imms) << 10 | U(rn) << 5 | U(rd);
}
static uint32_t
e_movw(int sf, int opc, int hw, uint32_t imm16, int rd)
{
    return U(sf) << 31 | U(opc) << 29 | 0x25u << 23 | U(hw) << 21 |
        (imm16 & 0xffff) << 5 | U(rd);
}
static uint32_t
e_bitfield(int sf, int opc, int immr, int imms, int rn, int rd)
{
    return U(sf) << 31 | U(opc) << 29 | 0x26u << 23 | U(sf) << 22 | U(immr) << 16 |
        U(imms) << 10 | U(rn) << 5 | U(rd);
}
static uint32_t
e_ldst_uimm(int size, int V, int opc, uint32_t imm12, int rn, int rt)
{
    return U(size) << 30 | 0x39u << 24 | U(V) << 26 | U(opc) << 22 |
        (imm12 & 0xfff) << 10 | U(rn) << 5 | U(rt);
}
/* type: 0 unscaled, 1 post, 3 pre, 2 unprivileged */
static uint32_t
e_ldst_imm9(int size, int V, int opc, int imm9, int type, int rn, int rt)
{
    return U(size) << 30 | 0x38u << 24 | U(V) << 26 | U(opc) << 22 |
        (U(imm9) & 0x1ff) << 12 | U(type) << 10 | U(rn) << 5 | U(rt);
}
static uint32_t
e_ldst_reg(int size, int V, int opc, int rm, int option, int S, int rn, int rt)
{
    return U(size) << 30 | 0x38u << 24 | U(V) << 26 | U(opc) << 22 | 1u << 21 |
        U(rm) << 16 | U(option) << 13 | U(S) << 12 | 2u << 10 | U(rn) << 5 | U(rt);
}
/* type: 0 non-temporal, 1 post, 2 offset, 3 pre */
static uint32_t
e_ldst_pair(int opc, int V, int type, int L, int imm7, int rt2, int rn, int rt)
{
    return U(opc) << 30 | 0x5u << 27 | U(V) << 26 | U(type) << 23 | U(L) << 22 |
        (U(imm7) & 0x7f) << 15 | U(rt2) << 10 | U(rn) << 5 | U(rt);
}
static uint32_t
e_excl(int size, int o2, int L, int o1, int rs, int o0, int rt2, int rn, int rt)
{
    return U(size) << 30 | 0x08u << 24 | U(o2) << 23 | U(L) << 22 | U(o1) << 21 |
        U(rs) << 16 | U(o0) << 15 | U(rt2) << 10 | U(rn) << 5 | U(rt);
}
static uint32_t
e_lse(int size, int A, int R, int rs, int o3, int opc, int rn, int rt)
{
    return U(size) << 30 | 0x38u << 24 | U(A) << 23 | U(R) << 22 | 1u << 21 |
        U(rs) << 16 | U(o3) << 15 | U(opc) << 12 | U(rn) << 5 | U(rt);
}
static uint32_t
e_mrs(uint32_t sysreg, int rt)
{
    return 0xd5300000u | sysreg << 5 | U(rt);
}
static uint32_t
e_msr(uint32_t sysreg, int rt)
{
    return 0xd5100000u | sysreg << 5 | U(rt);
}
#define SR(op0, op1, crn, crm, op2) \
    (U((op0) - 2) << 14 | U(op1) << 11 | U(crn) << 7 | U(crm) << 3 | U(op2))
#define SR_NZCV SR(3, 3, 4, 2, 0)
#define SR_FPCR SR(3, 3, 4, 4, 0)
#define SR_FPSR SR(3, 3, 4, 4, 1)
#define SR_DIT SR(3, 3, 4, 2, 5)
#define SR_TPIDR SR(3, 3, 13, 0, 2)
#define SR_TPIDRRO SR(3, 3, 13, 0, 3)
#define SR_CTR SR(3, 3, 0, 0, 1)
#define SR_DCZID SR(3, 3, 0, 0, 7)
#define SR_MIDR SR(3, 0, 0, 0, 0)
#define SR_ISAR0 SR(3, 0, 0, 6, 0)
#define SR_PFR0 SR(3, 0, 0, 4, 0)

/***************************************************************************
 * Generator state.
 */
typedef struct {
    int st;  /* 0 none, 1 signed with known original, 2 corrupted */
    int key; /* 0 IA, 1 IB, 2 DA, 3 DB */
    uint64_t mod, orig;
} pacst_t;

typedef struct {
    /* Configuration. */
    int allow_excl, allow_faults, allow_brk, allow_svc, allow_fpcr, allow_ctr;
    int traced;     /* will be single-stepped: no exclusives, no BRK */
    int xres, vres; /* reserved (never read/written) registers, or -1 */
    int len;        /* approx static top-level length */
    uint64_t dyn_max;
    /* State. */
    rng_t r;
    asm_t *a;
    int known[32];
    uint64_t val[32]; /* [31] is SP, always known */
    int lock[32];
    pacst_t pac[32];
    uint8_t sinit[STK_CMP];
    uint64_t ceiling;
    int depth;
    uint64_t mult, dyn;
    int lit_lab; /* literal pool label */
    uint64_t litval[8];
    int nfaults_planned;
    int wild_branch;    /* block may end via a wild (PAC-failed) branch */
    uint64_t fault_dyn; /* estimated dynamic count of planned faults */
    int has_excl;
    int ncas, ncas_primed; /* CAS/CASP emitted, and those primed to succeed */
    int exit_lab;
    uint64_t prot_lo, prot_hi; /* stack bytes holding a saved frame: never store */
    int dczva_ok;
} gen_t;

#define SPV (g->val[31])

static inline int
in_d(uint64_t a, uint64_t sz)
{
    return a >= D_ADDR && a + sz <= D_ADDR + D_SIZE;
}

static int
stk_readable(gen_t *g, uint64_t a, uint64_t sz)
{
    if (a < SPV || a + sz > STK_TOP)
        return 0;
    for (uint64_t i = 0; i < sz; i++) {
        if (!g->sinit[a + i - STK_CMP_LO])
            return 0;
    }
    return 1;
}

static void
stk_written(gen_t *g, uint64_t a, uint64_t sz)
{
    for (uint64_t i = 0; i < sz; i++) {
        uint64_t x = a + i;
        if (x >= STK_CMP_LO && x < STK_TOP && x >= SPV)
            g->sinit[x - STK_CMP_LO] = 1;
    }
}

/* SP moved from old to SPV: anything below the new SP is clobberable. */
static void
sp_moved(gen_t *g, uint64_t old)
{
    if (SPV > old) {
        for (uint64_t x = old; x < SPV; x++) {
            if (x >= STK_CMP_LO && x < STK_TOP)
                g->sinit[x - STK_CMP_LO] = 0;
        }
    }
}

static int
is_ptr(gen_t *g, int r)
{
    return g->known[r] &&
        (in_d(g->val[r], 1) || (g->val[r] >= SP_FLOOR && g->val[r] < STK_TOP));
}

static void
gkill(gen_t *g, int r)
{
    if (r == 31)
        return;
    g->known[r] = 0;
    g->pac[r].st = 0;
}

static void
setk(gen_t *g, int r, uint64_t v)
{
    if (r == 31)
        return;
    g->known[r] = 1;
    g->val[r] = v;
    g->pac[r].st = 0;
}

static const int reg_weight[31] = {
    3, 2, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, /* x0-x15 */
    2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 6, 2, 4     /* x16-x30 */
};

static int
usable(gen_t *g, int r)
{
    return r != g->xres;
}

/* A register the block may write. */
static int
pick_dst(gen_t *g)
{
    for (int tries = 0; tries < 100; tries++) {
        int tot = 0;
        for (int r = 0; r < 31; r++)
            tot += reg_weight[r];
        int x = (int)rn(&g->r, tot), r;
        for (r = 0; r < 31; r++) {
            x -= reg_weight[r];
            if (x < 0)
                break;
        }
        if (g->lock[r] || !usable(g, r))
            continue;
        if (is_ptr(g, r) && rp(&g->r, 60))
            continue;
        return r;
    }
    for (int r = 0; r < 31; r++) {
        if (!g->lock[r] && usable(g, r))
            return r;
    }
    return -1;
}

/* A source register; 31 is xzr (or sp in sp contexts). */
static int
pick_src(gen_t *g)
{
    if (rp(&g->r, 5))
        return 31;
    for (;;) {
        int tot = 0;
        for (int r = 0; r < 31; r++)
            tot += reg_weight[r];
        int x = (int)rn(&g->r, tot), r;
        for (r = 0; r < 31; r++) {
            x -= reg_weight[r];
            if (x < 0)
                break;
        }
        if (usable(g, r))
            return r;
    }
}

static int
pick_v(gen_t *g)
{
    for (;;) {
        int v = (int)rn(&g->r, 32);
        if (v != g->vres)
            return v;
    }
}

/* Materialize v into r with MOVZ/MOVK (or MOVN). */
static void
mov_imm(gen_t *g, int r, uint64_t v)
{
    asm_t *a = g->a;
    int first = 1;
    if ((~v >> 16) == 0) {
        emit(a, e_movw(1, 0, 0, U(~v & 0xffff), r), "movn %s, #0x%x", XR(r),
             U(~v & 0xffff));
        setk(g, r, v);
        g->dyn += g->mult;
        return;
    }
    for (int hw = 0; hw < 4; hw++) {
        uint32_t c = (v >> (16 * hw)) & 0xffff;
        if (c == 0 && !(first && hw == 3))
            continue;
        if (first) {
            emit(a, e_movw(1, 2, hw, c, r), "movz %s, #0x%x, lsl #%d", XR(r), c, 16 * hw);
            first = 0;
        } else
            emit(a, e_movw(1, 3, hw, c, r), "movk %s, #0x%x, lsl #%d", XR(r), c, 16 * hw);
        g->dyn += g->mult;
    }
    if (first)
        emit(a, e_movw(1, 2, 0, 0, r), "movz %s, #0", XR(r));
    setk(g, r, v);
}

/* Materialize a pointer, sometimes PC-relatively. */
static void
mov_ptr(gen_t *g, int r, uint64_t v)
{
    asm_t *a = g->a;
    if (rp(&g->r, 40)) {
        uint64_t pc = pc_of(a->n);
        int64_t pages = (int64_t)((v & ~0xfffULL) - (pc & ~0xfffULL)) >> 12;
        uint32_t imm = U(pages) & 0x1fffff;
        emit(a, 0x90000000u | (imm & 3) << 29 | ((imm >> 2) & 0x7ffff) << 5 | U(r),
             "adrp %s, 0x%llx", XR(r), (unsigned long long)(v & ~0xfffULL));
        if ((v & 0xfff) != 0 || rp(&g->r, 30)) {
            emit(a, e_addsub_imm(1, 0, 0, 0, U(v & 0xfff), r, r), "add %s, %s, #0x%x",
                 XS(r), XS(r), U(v & 0xfff));
        }
        setk(g, r, v);
        g->dyn += 2 * g->mult;
        return;
    }
    mov_imm(g, r, v);
}

/***************************************************************************
 * Knowledge snapshots for structured control flow.
 */
typedef struct {
    int known[32];
    uint64_t val[32];
    pacst_t pac[32];
    uint8_t sinit[STK_CMP];
    uint64_t ceiling;
} snap_t;

/* Signed-pointer knowledge cannot be recreated at a join point, so drop it
 * before entering a loop, conditional or subroutine body.
 */
static void
forget_pac(gen_t *g)
{
    for (int r = 0; r < 31; r++) {
        if (g->pac[r].st != 0)
            gkill(g, r);
    }
}

static void
snap(gen_t *g, snap_t *s)
{
    memcpy(s->known, g->known, sizeof(s->known));
    memcpy(s->val, g->val, sizeof(s->val));
    memcpy(s->pac, g->pac, sizeof(s->pac));
    memcpy(s->sinit, g->sinit, sizeof(s->sinit));
    s->ceiling = g->ceiling;
}

static void
sp_add(gen_t *g, int64_t delta)
{
    uint64_t old = SPV;
    if (delta == 0)
        return;
    uint64_t m = delta < 0 ? (uint64_t)-delta : (uint64_t)delta;
    while (m > 0) {
        uint32_t chunk = m > 0xff0 ? 0xff0 : U(m);
        emit(g->a, e_addsub_imm(1, delta < 0, 0, 0, chunk, 31, 31), "%s sp, sp, #0x%x",
             delta < 0 ? "sub" : "add", chunk);
        m -= chunk;
        g->dyn += g->mult;
    }
    g->val[31] = old + delta;
    sp_moved(g, old);
}

/* Restore every register that was known in s and has changed, so that the
 * state at a join point (loop head, if-join, return) matches s.
 */
static void
restore_to(gen_t *g, snap_t *s)
{
    if (SPV != s->val[31])
        sp_add(g, (int64_t)(s->val[31] - SPV));
    for (int r = 0; r < 31; r++) {
        if (s->known[r] &&
            !(g->known[r] && g->val[r] == s->val[r] && g->pac[r].st == 0)) {
            mov_imm(g, r, s->val[r]);
        } else if (s->pac[r].st == 1 &&
                   !(g->pac[r].st == 1 && g->pac[r].mod == s->pac[r].mod &&
                     g->pac[r].key == s->pac[r].key &&
                     g->pac[r].orig == s->pac[r].orig)) {
            /* Cannot recreate a signed value cheaply: forget it at the join. */
            s->pac[r].st = 0;
        }
    }
    /* Anything not known in s is unknown after the join. */
    for (int r = 0; r < 31; r++) {
        if (!s->known[r] && s->pac[r].st != 1)
            gkill(g, r);
        else if (!s->known[r] && s->pac[r].st == 1)
            g->pac[r] = s->pac[r], g->known[r] = 0;
    }
}

/***************************************************************************
 * Memory address planning.
 */
enum { F_U12, F_S9, F_P7, F_ZERO, F_S10X8 };

static int
imm_ok(int kind, int64_t off, int scale)
{
    switch (kind) {
    case F_U12: return off >= 0 && off % scale == 0 && off / scale < 4096;
    case F_S9: return off >= -256 && off <= 255;
    case F_P7: return off % scale == 0 && off / scale >= -64 && off / scale <= 63;
    case F_ZERO: return off == 0;
    case F_S10X8: return off % 8 == 0 && off >= -4096 && off <= 4088;
    }
    return 0;
}

/* Choose a target address.  region: 0 any valid, 1 guard page (fault),
 * 2 straddling the end of D into the guard (fault).  write: 0 read-only
 * access, 1 write-only access, 2 read-modify-write (atomics, exclusives).
 */
static int
plan_addr(gen_t *g, uint64_t size, uint64_t align, int write, int region, uint64_t *out)
{
    if (region == 1) {
        *out = (GUARD_ADDR + rn(&g->r, (uint32_t)(GUARD_SIZE - size))) & ~(align - 1);
        return 1;
    }
    if (region == 2) {
        if (size < 2)
            return 0;
        *out = D_ADDR + D_SIZE - size + 1 + rn(&g->r, (uint32_t)size - 1);
        if (*out & (align - 1))
            return 0;
        return 1;
    }
    for (int tries = 0; tries < 40; tries++) {
        uint64_t a;
        if (rp(&g->r, 30)) {
            if (STK_TOP - size < SPV)
                continue;
            a = SPV + rn(&g->r, (uint32_t)(STK_TOP - size - SPV + 1));
            a &= ~(align - 1);
            if (a < SPV)
                continue;
            if (write != 1 && !stk_readable(g, a, size))
                continue;
            if (write && a < g->prot_hi && a + size > g->prot_lo)
                continue;
        } else {
            /* Favour the ends of D. */
            if (rp(&g->r, 20))
                a = D_ADDR + D_SIZE - size - rn(&g->r, 64);
            else
                a = D_ADDR + rn(&g->r, (uint32_t)(D_SIZE - size + 1));
            a &= ~(align - 1);
            if (!in_d(a, size))
                continue;
        }
        *out = a;
        return 1;
    }
    return 0;
}

/* Find (or materialize) a base register so that addr == base + off with off
 * encodable in the given form.  Avoids av1/av2 as the base.  allow_sp: SP may
 * serve as the base.
 */
static int
find_base(gen_t *g, uint64_t addr, int kind, int scale, int allow_sp, int av1, int av2,
          int *base, int64_t *off)
{
    int cand[32], nc = 0;
    for (int r = 0; r < 32; r++) {
        if (r == 31 && !allow_sp)
            continue;
        if (r == av1 || r == av2 || !usable(g, r) || !g->known[r])
            continue;
        int64_t o = (int64_t)(addr - g->val[r]);
        if (imm_ok(kind, o, scale)) {
            cand[nc++] = r;
            if (r == 28)
                cand[nc++] = r; /* bias towards the stolen register */
        }
    }
    if (nc > 0 && rp(&g->r, 85)) {
        int r = cand[rn(&g->r, nc)];
        *base = r;
        *off = (int64_t)(addr - g->val[r]);
        return 1;
    }
    /* Materialize a fresh base. */
    for (int tries = 0; tries < 20; tries++) {
        int r = pick_dst(g);
        if (r < 0)
            return 0;
        if (r == av1 || r == av2)
            continue;
        int64_t o = 0;
        switch (kind) {
        case F_U12: o = (int64_t)rn(&g->r, 64) * scale; break;
        case F_S9: o = (int64_t)rn(&g->r, 512) - 256; break;
        case F_P7: o = ((int64_t)rn(&g->r, 128) - 64) * scale; break;
        case F_S10X8: o = ((int64_t)rn(&g->r, 1024) - 512) * 8; break;
        default: o = 0;
        }
        if (rp(&g->r, 40))
            o = 0;
        if (!imm_ok(kind, o, scale))
            o = 0;
        mov_ptr(g, r, addr - (uint64_t)o);
        *base = r;
        *off = o;
        return 1;
    }
    return 0;
}

static const char *
memtxt(int base, int64_t off, int wb)
{
    static __thread char buf[4][48];
    static __thread int i;
    char *b = buf[i++ & 3];
    if (wb == 2)
        snprintf(b, 48, "[%s], #%lld", XS(base), (long long)off);
    else if (wb == 1)
        snprintf(b, 48, "[%s, #%lld]!", XS(base), (long long)off);
    else if (off == 0)
        snprintf(b, 48, "[%s]", XS(base));
    else
        snprintf(b, 48, "[%s, #%lld]", XS(base), (long long)off);
    return b;
}

static void
set_mi(gen_t *g, int at, int base, int64_t off, int wb, int must)
{
    meminfo_t *m = &g->a->mi[at];
    m->valid = 1;
    m->base = (uint8_t)base;
    m->idx = -1;
    m->off = (int32_t)off;
    m->wb = (uint8_t)wb;
    m->must = (uint8_t)must;
}

/* Records the access kind and size of the memory instruction at index at. */
static void
set_acc(asm_t *a, int at, int acc, uint64_t size)
{
    a->mi[at].acc = (uint8_t)acc;
    a->mi[at].size = (uint16_t)size;
}

/* Plan a fault for an access of this size: 0 none. */
static int
fault_budget(gen_t *g)
{
    return g->allow_faults && g->fault_dyn + g->mult <= 24;
}

static int
fault_region(gen_t *g, uint64_t size)
{
    if (!fault_budget(g) || !rp(&g->r, 6))
        return 0;
    if (size > 1 && rp(&g->r, 30))
        return 2;
    return 1;
}

static void
note_fault(gen_t *g, int at)
{
    g->a->fault_ok[at] = 1;
    g->nfaults_planned++;
    g->fault_dyn += g->mult;
}

/* Base update for writeback. */
static int
wb_ok(gen_t *g, int base, int64_t newval_delta)
{
    if (base != 31)
        return 1;
    uint64_t nsp = SPV + newval_delta;
    return newval_delta % 16 == 0 && nsp >= SP_FLOOR && nsp <= g->ceiling;
}

static void
apply_wb(gen_t *g, int base, int64_t delta)
{
    if (base == 31) {
        uint64_t old = SPV;
        g->val[31] += delta;
        sp_moved(g, old);
    } else
        g->val[base] += delta;
}

/***************************************************************************
 * Single register loads and stores.
 */
static void
gen_ldst_single(gen_t *g)
{
    asm_t *a = g->a;
    int V = rp(&g->r, 25);
    int sizelog = V ? (int)rn(&g->r, 5) : (int)rn(&g->r, 4);
    uint64_t size = 1ULL << sizelog;
    int load = rp(&g->r, 55);
    int sext = 0, sext32 = 0;
    if (!V && load && sizelog < 3 && rp(&g->r, 30)) {
        sext = 1;
        sext32 = sizelog < 2 && rp(&g->r, 40);
    }
    int fr = fault_region(g, size);
    uint64_t addr;
    if (!plan_addr(g, size, 1, !load, fr, &addr))
        return;
    /* form: 0 uimm12, 1 unscaled, 2 pre, 3 post, 4 reg, 5 unprivileged */
    int form = (int)rn(&g->r, 6);
    if (V && form == 5)
        form = 1;
    int encsize = V ? (sizelog == 4 ? 0 : sizelog) : sizelog;
    int opc;
    if (V)
        opc = (sizelog == 4 ? 2 : 0) | load;
    else if (!load)
        opc = 0;
    else if (!sext)
        opc = 1;
    else if (sizelog == 2)
        opc = 2;
    else
        opc = sext32 ? 3 : 2;
    int rt = -1, base;
    int64_t off;
    const char *mn;
    char rtn[8];
    static const char *ldn[] = { "ldrb", "ldrh", "ldr", "ldr" };
    static const char *stn[] = { "strb", "strh", "str", "str" };
    static const char *ldsn[] = { "ldrsb", "ldrsh", "ldrsw", "?" };
    if (V) {
        mn = load ? "ldr" : "str";
        snprintf(rtn, sizeof(rtn), "%c%d", "bhsdq"[sizelog], 0);
    } else
        mn = load ? (sext ? ldsn[sizelog] : ldn[sizelog]) : stn[sizelog];
    if (form == 4) {
        /* Register offset. */
        int shift = rp(&g->r, 50) ? sizelog : 0;
        static const int exts[] = { 2, 3, 6, 7 };
        int ext = exts[rn(&g->r, 4)];
        int cand[32], nc = 0;
        for (int r = 0; r < 32; r++) {
            if (!g->known[r] || !usable(g, r))
                continue;
            int64_t d = (int64_t)(addr - g->val[r]);
            if (d % (1 << shift) != 0)
                continue;
            int64_t k = d >> shift;
            if ((ext == 2 && (k < 0 || k > 0xffffffffLL)) ||
                (ext == 6 && (k < INT32_MIN || k > INT32_MAX)))
                continue;
            cand[nc++] = r;
        }
        if (nc == 0)
            return;
        base = cand[rn(&g->r, nc)];
        int64_t k = (int64_t)(addr - g->val[base]) >> shift;
        int idx;
        if (k == 0 && rp(&g->r, 30))
            idx = 31;
        else {
            /* Pick an unlocked index register other than the base. */
            idx = -1;
            for (int t = 0; t < 20 && idx < 0; t++) {
                int r = pick_dst(g);
                if (r >= 0 && r != base)
                    idx = r;
            }
            if (idx < 0)
                return;
            if (ext == 2 || ext == 6) {
                /* Only the low 32 bits matter: put junk above them. */
                uint64_t v = (uint64_t)(uint32_t)k | (rnext(&g->r) << 32);
                mov_imm(g, idx, v);
            } else
                mov_imm(g, idx, (uint64_t)k);
        }
        if (load && !V) {
            rt = pick_dst(g);
            if (rt < 0)
                return;
        } else if (!V)
            rt = pick_src(g);
        else
            rt = load ? pick_v(g) : (int)rn(&g->r, 32);
        if (g->vres >= 0 && V && rt == g->vres)
            return;
        static const char *extn[8] = { "", "", "uxtw", "lsl", "", "", "sxtw", "sxtx" };
        int at = emit(a, e_ldst_reg(encsize, V, opc, idx, ext, shift ? 1 : 0, base, rt),
                      "%s %s, [%s, %s, %s #%d]", mn,
                      V ? (snprintf(rtn, sizeof(rtn), "%c%d", "bhsdq"[sizelog], rt), rtn)
                        : GR(sizelog == 3 || (sext && !sext32), rt),
                      XS(base), (ext == 2 || ext == 6) ? WR(idx) : XR(idx), extn[ext],
                      shift ? sizelog : 0);
        set_mi(g, at, base, 0, 0, 1);
        set_acc(a, at, load ? MA_R : MA_W, size);
        a->mi[at].idx = (int8_t)idx;
        a->mi[at].ext = (uint8_t)ext;
        a->mi[at].shift = (uint8_t)(shift ? sizelog : 0);
        g->dyn += g->mult;
        if (fr)
            note_fault(g, at);
        if (load && !V)
            gkill(g, rt);
        if (!fr && !load && addr >= STK_CMP_LO)
            stk_written(g, addr, size);
        return;
    }
    int kind = form == 0 ? F_U12 : F_S9;
    int wb = form == 2 ? 1 : form == 3 ? 2 : 0;
    /* For post-index the access is at the base itself. */
    int64_t wbamt = 0;
    if (wb == 2) {
        if (!find_base(g, addr, F_ZERO, 1, 1, -1, -1, &base, &off))
            return;
        wbamt = (int64_t)rn(&g->r, 512) - 256;
        if (base == 31)
            wbamt &= ~15LL;
        if (!wb_ok(g, base, wbamt))
            wbamt = 0;
        if (base == 31 && !wb_ok(g, base, wbamt))
            return;
    } else {
        if (!find_base(g, addr, kind, (int)size, 1, -1, -1, &base, &off))
            return;
        if (wb == 1 && !wb_ok(g, base, off))
            return;
    }
    if (wb && base != 31 && g->lock[base])
        return;
    if (V) {
        rt = load ? pick_v(g) : (int)rn(&g->r, 32);
        if (g->vres >= 0 && rt == g->vres)
            return;
        snprintf(rtn, sizeof(rtn), "%c%d", "bhsdq"[sizelog], rt);
    } else {
        for (int t = 0; t < 20; t++) {
            rt = load ? pick_dst(g) : pick_src(g);
            if (rt < 0)
                return;
            if (!(wb && rt == base))
                break;
            rt = -1;
        }
        if (rt < 0)
            return;
        snprintf(rtn, sizeof(rtn), "%s", GR(sizelog == 3 || (sext && !sext32), rt));
    }
    uint32_t w;
    int64_t encoff = wb == 2 ? wbamt : off;
    if (form == 0)
        w = e_ldst_uimm(encsize, V, opc, U(off / (int64_t)size), base, rt);
    else
        w = e_ldst_imm9(encsize, V, opc, (int)encoff,
                        form == 1       ? 0
                            : form == 2 ? 3
                            : form == 3 ? 1
                                        : 2,
                        base, rt);
    const char *m2 = mn;
    char mbuf[16];
    if (form == 1 || form == 5) {
        /* ldur/ldurb/ldursb..., ldtr/ldtrb... */
        snprintf(mbuf, sizeof(mbuf), "%c%c%c%s", mn[0], mn[1], form == 1 ? 'u' : 't',
                 mn + 2);
        m2 = mbuf;
    }
    int at = emit(a, w, "%s %s, %s", m2, rtn, memtxt(base, encoff, wb));
    set_mi(g, at, base, wb == 2 ? 0 : off, wb, 1);
    set_acc(a, at, load ? MA_R : MA_W, size);
    g->dyn += g->mult;
    if (fr) {
        note_fault(g, at);
        if (load && !V)
            gkill(g, rt);
        return;
    }
    if (wb)
        apply_wb(g, base, wb == 1 ? off : wbamt);
    if (load && !V)
        gkill(g, rt);
    if (!load && addr >= STK_CMP_LO)
        stk_written(g, addr, size);
}

/***************************************************************************
 * Pair loads and stores.
 */
static void
gen_ldst_pair(gen_t *g)
{
    asm_t *a = g->a;
    int V = rp(&g->r, 30);
    int opc, sizelog;
    int load = rp(&g->r, 55);
    int ldpsw = 0;
    if (V) {
        opc = (int)rn(&g->r, 3);
        sizelog = 2 + opc;
    } else {
        int t = (int)rn(&g->r, 5);
        if (t == 4 && load) {
            opc = 1;
            sizelog = 2;
            ldpsw = 1;
        } else {
            opc = rp(&g->r, 50) ? 2 : 0;
            sizelog = opc ? 3 : 2;
        }
    }
    uint64_t size = 1ULL << sizelog;
    int fr = fault_region(g, 2 * size);
    /* A load of several registers that faults partway may already have written
     * some of them (their values are UNKNOWN): only fault on the first access.
     */
    if (fr == 2 && load)
        fr = 1;
    uint64_t addr;
    if (!plan_addr(g, 2 * size, 1, !load, fr, &addr))
        return;
    int type = (int)rn(&g->r, 4); /* 0 nt, 1 post, 2 offset, 3 pre */
    if (ldpsw && type == 0)
        type = 2;
    int wb = type == 3 ? 1 : type == 1 ? 2 : 0;
    int base;
    int64_t off, wbamt = 0;
    if (wb == 2) {
        if (!find_base(g, addr, F_ZERO, 1, 1, -1, -1, &base, &off))
            return;
        wbamt = ((int64_t)rn(&g->r, 128) - 64) * (int64_t)size;
        if (base == 31)
            wbamt &= ~15LL;
        if (!wb_ok(g, base, wbamt))
            return;
    } else {
        if (!find_base(g, addr, F_P7, (int)size, 1, -1, -1, &base, &off))
            return;
        if (wb == 1 && !wb_ok(g, base, off))
            return;
    }
    if (wb && base != 31 && g->lock[base])
        return;
    int rt, rt2;
    char t1[8], t2[8];
    if (V) {
        rt = pick_v(g);
        rt2 = pick_v(g);
        if (load && rt == rt2)
            return;
        if (!load && g->vres >= 0)
            ; /* stores may read any V, except the reserved one */
        if (rt == g->vres || rt2 == g->vres)
            return;
        snprintf(t1, 8, "%c%d", "sdq"[opc], rt);
        snprintf(t2, 8, "%c%d", "sdq"[opc], rt2);
    } else {
        rt = load ? pick_dst(g) : pick_src(g);
        rt2 = load ? pick_dst(g) : pick_src(g);
        if (rt < 0 || rt2 < 0 || (load && rt == rt2))
            return;
        if (wb && (rt == base || rt2 == base))
            return;
        snprintf(t1, 8, "%s", GR(sizelog == 3 || ldpsw, rt));
        snprintf(t2, 8, "%s", GR(sizelog == 3 || ldpsw, rt2));
    }
    int64_t enc = wb == 2 ? wbamt : off;
    uint32_t w =
        e_ldst_pair(opc, V, type, load, (int)(enc / (int64_t)size), rt2, base, rt);
    const char *mn = ldpsw ? "ldpsw"
        : type == 0        ? (load ? "ldnp" : "stnp")
                           : (load ? "ldp" : "stp");
    int at = emit(a, w, "%s %s, %s, %s", mn, t1, t2, memtxt(base, enc, wb));
    set_mi(g, at, base, wb == 2 ? 0 : off, wb, 1);
    set_acc(a, at, load ? MA_R : MA_W, 2 * size);
    g->dyn += g->mult;
    if (fr) {
        note_fault(g, at);
        if (load && !V) {
            gkill(g, rt);
            gkill(g, rt2);
        }
        return;
    }
    if (wb)
        apply_wb(g, base, wb == 1 ? off : wbamt);
    if (load && !V) {
        gkill(g, rt);
        gkill(g, rt2);
    }
    if (!load && addr >= STK_CMP_LO)
        stk_written(g, addr, 2 * size);
}

/***************************************************************************
 * SIMD structure loads/stores.
 */
static void
gen_simd_struct(gen_t *g)
{
    asm_t *a = g->a;
    int load = rp(&g->r, 55);
    int Q = (int)rn(&g->r, 2);
    int esz = (int)rn(&g->r, 4);
    int kind = (int)rn(&g->r, 3); /* 0 multiple, 1 single lane, 2 replicate */
    if (kind == 2 && !load)
        kind = 1;
    int nregs, opcode = 0;
    uint64_t size;
    if (kind == 0) {
        static const int ops[] = { 7, 10, 6, 2, 8, 4, 0 }; /* LD1x1..4, LD2, LD3, LD4 */
        static const int nr[] = { 1, 2, 3, 4, 2, 3, 4 };
        int t = (int)rn(&g->r, 7);
        if (esz == 3 && !Q && t >= 4)
            t = 0; /* LD2-4 .1D is reserved */
        opcode = ops[t];
        nregs = nr[t];
        size = (uint64_t)nregs * (Q ? 16 : 8);
    } else {
        nregs = 1 + (int)rn(&g->r, 4);
        size = (uint64_t)nregs << esz;
    }
    int fr = fault_region(g, size);
    if (fr == 2 && load)
        fr = 1; /* see gen_ldst_pair() */
    uint64_t addr;
    if (!plan_addr(g, size, 1, !load, fr, &addr))
        return;
    int base;
    int64_t off;
    if (!find_base(g, addr, F_ZERO, 1, 1, -1, -1, &base, &off))
        return;
    int post = (int)rn(&g->r, 3); /* 0 none, 1 imm, 2 reg */
    int rm = 31;
    int64_t wbamt = 0;
    if (post == 1)
        wbamt = (int64_t)size;
    else if (post == 2) {
        rm = pick_src(g);
        if (rm == 31 || rm == base || !g->known[rm]) {
            post = 1;
            wbamt = (int64_t)size;
            rm = 31;
        } else
            wbamt = (int64_t)g->val[rm];
    }
    if (post && !wb_ok(g, base, wbamt)) {
        if (base == 31)
            return;
    }
    if (post && (base == 31 || g->lock[base]))
        return; /* keep SP aligned/simple */
    int vt = pick_v(g);
    for (int i = 0; i < nregs; i++) {
        if (((vt + i) & 31) == g->vres)
            return;
    }
    uint32_t w;
    char lane[24] = "";
    static const char *arr[4][2] = {
        { "8b", "16b" }, { "4h", "8h" }, { "2s", "4s" }, { "1d", "2d" }
    };
    static const char *ln[4] = { "b", "h", "s", "d" };
    if (kind == 0) {
        w = U(Q) << 30 | 0x0Cu << 24 | U(load) << 22 | U(opcode) << 12 | U(esz) << 10 |
            U(base) << 5 | U(vt);
        if (post)
            w |= 1u << 23 | U(rm) << 16;
        snprintf(lane, sizeof(lane), ".%s", arr[esz][Q]);
    } else {
        /* Single structure: LDn/STn (lane) or LDnR. */
        int S = 0, sz = 0, idx = 0, op3;
        int R = (nregs == 2 || nregs == 4) ? 1 : 0;
        int sel = (nregs == 1 || nregs == 2) ? 0 : 1; /* opcode<0> for 3/4 regs */
        if (kind == 2) {
            op3 = 6 | sel;
            sz = esz;
            snprintf(lane, sizeof(lane), ".%s", arr[esz][Q]);
            w = U(Q) << 30 | 0x0Du << 24 | U(load) << 22 | U(R) << 21 | U(op3) << 13 |
                U(sz) << 10 | U(base) << 5 | U(vt);
        } else {
            switch (esz) {
            case 0:
                idx = (int)rn(&g->r, 16);
                op3 = 0 | sel;
                break;
            case 1:
                idx = (int)rn(&g->r, 8);
                op3 = 2 | sel;
                break;
            case 2:
                idx = (int)rn(&g->r, 4);
                op3 = 4 | sel;
                break;
            default:
                idx = (int)rn(&g->r, 2);
                op3 = 4 | sel;
                break;
            }
            int q2 = 0;
            switch (esz) {
            case 0:
                q2 = idx >> 3;
                S = (idx >> 2) & 1;
                sz = idx & 3;
                break;
            case 1:
                q2 = idx >> 2;
                S = (idx >> 1) & 1;
                sz = (idx & 1) << 1;
                break;
            case 2:
                q2 = idx >> 1;
                S = idx & 1;
                sz = 0;
                break;
            default:
                q2 = idx;
                S = 0;
                sz = 1;
                break;
            }
            Q = q2;
            snprintf(lane, sizeof(lane), ".%s[%d]", ln[esz], idx);
            w = U(Q) << 30 | 0x0Du << 24 | U(load) << 22 | U(R) << 21 | U(op3) << 13 |
                U(S) << 12 | U(sz) << 10 | U(base) << 5 | U(vt);
        }
        if (post)
            w |= 1u << 23 | U(rm) << 16;
    }
    char ptxt[24] = "";
    if (post == 1)
        snprintf(ptxt, sizeof(ptxt), ", #%lld", (long long)wbamt);
    else if (post == 2)
        snprintf(ptxt, sizeof(ptxt), ", %s", XR(rm));
    const char *mnb = load ? "ld" : "st";
    int nst = kind == 0 ? (opcode == 8       ? 2
                               : opcode == 4 ? 3
                               : opcode == 0 ? 4
                                             : 1)
                        : nregs;
    int at = emit(a, w, "%s%d%s {v%d%s +%d}, [%s]%s", mnb, nst, kind == 2 ? "r" : "", vt,
                  lane, nregs - 1, XS(base), ptxt);
    set_mi(g, at, base, 0, post ? 2 : 0, 1);
    set_acc(a, at, load ? MA_R : MA_W, size);
    g->dyn += g->mult;
    if (fr) {
        note_fault(g, at);
        return;
    }
    if (post)
        apply_wb(g, base, wbamt);
    if (!load && addr >= STK_CMP_LO)
        stk_written(g, addr, size);
}

/***************************************************************************
 * Acquire/release and LSE atomics.
 */
static void
gen_atomic(gen_t *g)
{
    asm_t *a = g->a;
    int t = (int)rn(&g->r, 10);
    if (!feat.lse && t >= 3)
        t = (int)rn(&g->r, 3);
    if (!feat.lrcpc && t == 2)
        t = 0;
    int sizelog = (int)rn(&g->r, 4);
    uint64_t size = 1ULL << sizelog;
    int casp = t == 4;
    if (casp) {
        sizelog = 2 + (int)rn(&g->r, 2);
        size = 2ULL << sizelog;
    }
    int load = t == 0 || t == 2 || t >= 3;
    int fr = fault_region(g, size);
    if (fr == 2)
        fr = 1;
    uint64_t addr;
    /* Misaligned atomics: with FEAT_LSE2 they work inside a 16-byte granule
     * and raise an alignment fault across one, either of which is deterministic:
     * treat as a possible fault in D only.
     */
    int unaligned = !fr && size > 1 && fault_budget(g) && rp(&g->r, 8);
    if (unaligned) {
        addr = D_ADDR + rn(&g->r, (uint32_t)(D_SIZE - size)) + 0;
        if (addr % size == 0)
            addr++;
        if (!in_d(addr, size))
            return;
        fr = 3;
    } else if (!plan_addr(g, size, size, t == 1 ? 1 : t >= 3 ? 2 : 0, fr, &addr))
        return;
    int base;
    int64_t off;
    if (!find_base(g, addr, F_ZERO, 1, 1, -1, -1, &base, &off))
        return;
    int sf = sizelog == 3;
    int at;
    int dst1 = -1, dst2 = -1;
    if (t == 0 || t == 1) {
        /* LDAR / STLR (also LDLAR/STLLR are LOR: skip). */
        int rt = t == 0 ? pick_dst(g) : pick_src(g);
        if (rt < 0)
            return;
        at = emit(a, e_excl(sizelog, 1, t == 0, 0, 31, 1, 31, base, rt), "%s%s %s, [%s]",
                  t == 0 ? "ldar" : "stlr",
                  sizelog == 0       ? "b"
                      : sizelog == 1 ? "h"
                                     : "",
                  GR(sf, rt), XS(base));
        if (t == 0)
            dst1 = rt;
    } else if (t == 2) {
        int rt = pick_dst(g);
        if (rt < 0)
            return;
        at = emit(a, 0x38bfc000u | U(sizelog) << 30 | U(base) << 5 | U(rt),
                  "ldapr%s %s, [%s]",
                  sizelog == 0       ? "b"
                      : sizelog == 1 ? "h"
                                     : "",
                  GR(sf, rt), XS(base));
        dst1 = rt;
    } else if (t == 3) {
        /* CAS{A}{L} Rs, Rt, [Xn] */
        int rs = pick_dst(g), rt = pick_src(g);
        if (rs < 0)
            return;
        int A = (int)rn(&g->r, 2), L = (int)rn(&g->r, 2);
        if (rp(&g->r, 40) && !fr && base != 31 && rs != base) {
            /* Make the compare succeed: load the current value as the expected one. */
            int ld = emit(a, e_ldst_uimm(sizelog, 0, 1, 0, base, rs), "ldr%s %s, [%s]",
                          sizelog == 0       ? "b"
                              : sizelog == 1 ? "h"
                                             : "",
                          GR(sf, rs), XS(base));
            set_mi(g, ld, base, 0, 0, 1);
            set_acc(a, ld, MA_R, size);
            g->dyn += g->mult;
            g->ncas_primed++;
        }
        g->ncas++;
        at = emit(a, e_excl(sizelog, 1, A, 1, rs, L, 31, base, rt),
                  "cas%s%s%s %s, %s, [%s]", A ? "a" : "", L ? "l" : "",
                  sizelog == 0       ? "b"
                      : sizelog == 1 ? "h"
                                     : "",
                  GR(sf, rs), GR(sf, rt), XS(base));
        dst1 = rs;
    } else if (t == 4) {
        /* CASP: even register pairs. */
        int rs = 2 * (int)rn(&g->r, 15), rt = 2 * (int)rn(&g->r, 15);
        if (g->lock[rs] || g->lock[rs + 1] || !usable(g, rs) || !usable(g, rs + 1) ||
            !usable(g, rt) || !usable(g, rt + 1))
            return;
        int A = (int)rn(&g->r, 2), L = (int)rn(&g->r, 2);
        int psf = sizelog == 3;
        if (rp(&g->r, 40) && !fr && base != 31 && rs != base && rs + 1 != base) {
            /* Make the compare succeed: load the current pair as the expected one. */
            int ld = emit(a, e_ldst_pair(psf ? 2 : 0, 0, 2, 1, 0, rs + 1, base, rs),
                          "ldp %s, %s, [%s]", GR(psf, rs), GR(psf, rs + 1), XS(base));
            set_mi(g, ld, base, 0, 0, 1);
            set_acc(a, ld, MA_R, size);
            g->dyn += g->mult;
            g->ncas_primed++;
        }
        g->ncas++;
        at = emit(a, e_excl(psf, 0, A, 1, rs, L, 31, base, rt),
                  "casp%s%s %s, %s, %s, %s, [%s]", A ? "a" : "", L ? "l" : "",
                  GR(psf, rs), GR(psf, rs + 1), GR(psf, rt), GR(psf, rt + 1), XS(base));
        dst1 = rs;
        dst2 = rs + 1;
    } else {
        /* SWP / LD<op> / ST<op> */
        int o3 = t == 5;
        int opc = o3 ? 0 : (int)rn(&g->r, 8);
        int rs = pick_src(g);
        int rt = rp(&g->r, 20) && !o3 ? 31 : pick_dst(g);
        if (rt < 0)
            return;
        int A = rt == 31 ? 0 : (int)rn(&g->r, 2), R = (int)rn(&g->r, 2);
        static const char *ops[] = { "add",  "clr",  "eor",  "set",
                                     "smax", "smin", "umax", "umin" };
        at = emit(a, e_lse(sizelog, A, R, rs, o3, opc, base, rt),
                  "%s%s%s%s%s %s, %s, [%s]",
                  o3 ? "swp"
                      : rt == 31 ? "st"
                                 : "ld",
                  o3 ? "" : ops[opc], A ? "a" : "", R ? "l" : "",
                  sizelog == 0       ? "b"
                      : sizelog == 1 ? "h"
                                     : "",
                  GR(sf, rs), GR(sf, rt), XS(base));
        if (rt != 31)
            dst1 = rt;
    }
    set_mi(g, at, base, 0, 0, 1);
    set_acc(a, at, t == 0 || t == 2 ? MA_R : t == 1 ? MA_W : MA_RW, size);
    g->dyn += g->mult;
    if (fr)
        note_fault(g, at);
    if (dst1 >= 0)
        gkill(g, dst1);
    if (dst2 >= 0)
        gkill(g, dst2);
    if (!fr && t != 0 && t != 2 && addr >= STK_CMP_LO)
        stk_written(g, addr, size);
}

/* An LDXR/STXR retry loop. */
static void
gen_excl_loop(gen_t *g)
{
    asm_t *a = g->a;
    int pair = rp(&g->r, 25);
    int sizelog = pair ? 2 + (int)rn(&g->r, 2) : (int)rn(&g->r, 4);
    uint64_t size = pair ? 2ULL << sizelog : 1ULL << sizelog;
    int fr = fault_budget(g) && rp(&g->r, 8);
    uint64_t addr;
    if (!plan_addr(g, size, size, 2, fr ? 1 : 0, &addr))
        return;
    if (fr && rp(&g->r, 50)) {
        /* A misaligned exclusive load: an alignment fault (SIGBUS). */
        addr = D_ADDR + 1 + rn(&g->r, (uint32_t)(D_SIZE - 2 * size));
        if (addr % size == 0)
            addr++;
    }
    int base;
    int64_t off;
    if (!find_base(g, addr, F_ZERO, 1, 1, -1, -1, &base, &off))
        return;
    int sf = sizelog == 3;
    int acq = (int)rn(&g->r, 2), rel = (int)rn(&g->r, 2);
    int rt = pick_dst(g), rt2 = pair ? pick_dst(g) : 31, rs = pick_dst(g),
        st = pick_dst(g);
    if (rt < 0 || rt2 < 0 || rs < 0 || st < 0)
        return;
    if (rt == base || st == base || st == rs || st == rt || rs == base ||
        (pair && (rt2 == base || rt2 == rt || rt2 == st || rt2 == rs)) || rt == rs)
        return;
    g->lock[base]++;
    int retry = lab_new(a);
    lab_bind(a, retry);
    const char *szs = pair ? "p" : sizelog == 0 ? "rb" : sizelog == 1 ? "rh" : "r";
    int at =
        emit(a, e_excl(pair ? (sf ? 3 : 2) : sizelog, 0, 1, pair, 31, acq, rt2, base, rt),
             "ld%sx%s %s%s%s, [%s]", acq ? "a" : "", szs, GR(sf, rt), pair ? ", " : "",
             pair ? GR(sf, rt2) : "", XS(base));
    set_mi(g, at, base, 0, 0, 1);
    set_acc(a, at, MA_R, size);
    if (fr) {
        /* Only the load: whether a store-exclusive without a monitor checks the
         * address is IMPLEMENTATION DEFINED.
         */
        note_fault(g, at);
        emit(a, 0xd503305fu, "clrex");
        g->lock[base]--;
        gkill(g, rt);
        if (pair)
            gkill(g, rt2);
        g->dyn += 2 * g->mult;
        return;
    }
    /* rs = f(rt) */
    if (rp(&g->r, 50)) {
        uint32_t imm = 1 + rn(&g->r, 100);
        emit(a, e_addsub_imm(1, 0, 0, 0, imm, rt, rs), "add %s, %s, #%u", XR(rs), XR(rt),
             imm);
    } else {
        int rm = pick_src(g);
        /* rs and st change when the store-exclusive fails and the loop retries
         * (which it can spuriously, e.g. on an interrupt): reading either would
         * make the stored value depend on the number of retries.
         */
        if (rm == rs || rm == st)
            rm = rt;
        emit(a, U(1) << 31 | U(2) << 29 | 0x0Au << 24 | U(rm) << 16 | U(rt) << 5 | U(rs),
             "eor %s, %s, %s", XR(rs), XR(rt), XR(rm));
    }
    at = emit(a,
              e_excl(pair ? (sf ? 3 : 2) : sizelog, 0, 0, pair, st, rel, pair ? rt2 : 31,
                     base, rs),
              "st%sx%s %s, %s%s%s, [%s]", rel ? "l" : "", szs, WR(st), GR(sf, rs),
              pair ? ", " : "", pair ? GR(sf, rt2) : "", XS(base));
    set_mi(g, at, base, 0, 0, 1);
    set_acc(a, at, MA_W, size);
    int cb = emit(a, 0x35000000u | U(st), "cbnz %s, L%d", WR(st), retry);
    fixup(a, cb, retry, FX_B19);
    a->excl_skip[a->lab[retry]] = (int16_t)(cb + 1);
    g->lock[base]--;
    gkill(g, rt);
    gkill(g, rs);
    gkill(g, st);
    if (pair)
        gkill(g, rt2);
    g->has_excl = 1;
    g->dyn += 4 * g->mult;
    if (addr >= STK_CMP_LO)
        stk_written(g, addr, size);
}

/* Literal loads from the block's pool. */
static void
gen_literal(gen_t *g)
{
    asm_t *a = g->a;
    int i = (int)rn(&g->r, 8);
    uint64_t litaddr = pc_of(a->lab[g->lit_lab]) + 8ULL * i;
    int64_t d = ((int64_t)litaddr - (int64_t)pc_of(a->n)) / 4;
    int t = (int)rn(&g->r, 6);
    int at;
    uint64_t lsize = 0;
    if (t <= 1) {
        lsize = t ? 8 : 4;
        int rt = pick_dst(g);
        if (rt < 0)
            return;
        at = emit(a, U(t) << 30 | 0x18u << 24 | (U(d) & 0x7ffff) << 5 | U(rt),
                  "ldr %s, =0x%llx (lit%d)", GR(t, rt), (unsigned long long)g->litval[i],
                  i);
        if (t == 1)
            setk(g, rt, g->litval[i]);
        else
            setk(g, rt, g->litval[i] & 0xffffffffULL);
    } else if (t == 2) {
        lsize = 4;
        int rt = pick_dst(g);
        if (rt < 0)
            return;
        at = emit(a, 2u << 30 | 0x18u << 24 | (U(d) & 0x7ffff) << 5 | U(rt),
                  "ldrsw %s, lit%d", XR(rt), i);
        gkill(g, rt);
    } else if (t == 3) {
        at = emit(a, 3u << 30 | 0x18u << 24 | (U(d) & 0x7ffff) << 5 | rn(&g->r, 32),
                  "prfm #x, lit%d", i);
        a->mi[at].valid = 1;
        a->mi[at].lit = 1;
        a->mi[at].litaddr = litaddr;
        a->mi[at].idx = -1;
        set_acc(a, at, MA_PRFM, 0);
        g->dyn += g->mult;
        return;
    } else {
        int opc = t - 4 + (rp(&g->r, 30) && i < 7 ? 1 : 0); /* S, D, or Q */
        int vt = pick_v(g);
        if (opc == 2 && i == 7)
            opc = 1;
        lsize = 4ULL << opc;
        at = emit(a, U(opc) << 30 | 0x1Cu << 24 | (U(d) & 0x7ffff) << 5 | U(vt),
                  "ldr %c%d, lit%d", "sdq"[opc], vt, i);
    }
    a->mi[at].valid = 1;
    a->mi[at].lit = 1;
    a->mi[at].must = 1;
    a->mi[at].litaddr = litaddr;
    a->mi[at].idx = -1;
    set_acc(a, at, MA_R, lsize);
    g->dyn += g->mult;
}

/* LDRAA/LDRAB: load through a DA/DB-signed pointer. */
static void
gen_ldraa(gen_t *g)
{
    asm_t *a = g->a;
    if (!feat.pauth)
        return;
    uint64_t addr;
    if (!plan_addr(g, 8, 8, 0, 0, &addr))
        return;
    int base = -1;
    int64_t off;
    int M = (int)rn(&g->r, 2);
    for (int r = 0; r < 31; r++) {
        if (g->pac[r].st == 1 && g->pac[r].key == 2 + M && g->pac[r].mod == 0 &&
            usable(g, r) && !g->lock[r] &&
            imm_ok(F_S10X8, (int64_t)(addr - g->pac[r].orig), 8)) {
            base = r;
            break;
        }
    }
    if (base < 0) {
        if (!find_base(g, addr, F_S10X8, 8, 0, -1, -1, &base, &off))
            return;
        if (g->lock[base] || base == 31)
            return;
        uint64_t orig = g->val[base];
        emit(a, 0xDAC10000u | U(10 + M) << 10 | 31u << 5 | U(base), "pacdz%c %s",
             M ? 'b' : 'a', XR(base));
        g->known[base] = 0;
        g->pac[base].st = 1;
        g->pac[base].key = 2 + M;
        g->pac[base].mod = 0;
        g->pac[base].orig = orig;
        g->dyn += g->mult;
    }
    off = (int64_t)(addr - g->pac[base].orig);
    int W = rp(&g->r, 30);
    int rt = pick_dst(g);
    if (rt < 0 || (W && rt == base))
        return;
    int64_t s10 = off / 8;
    uint32_t w = 0xF8200400u | U(M) << 23 | U((s10 >> 9) & 1) << 22 |
        (U(s10) & 0x1ff) << 12 | U(W) << 11 | U(base) << 5 | U(rt);
    int at = emit(a, w, "ldra%c %s, [%s, #%lld]%s", M ? 'b' : 'a', XR(rt), XR(base),
                  (long long)off, W ? "!" : "");
    set_mi(g, at, base, off, W, 1);
    set_acc(a, at, MA_R, 8);
    a->mi[at].pac = 1;
    g->dyn += g->mult;
    uint64_t orig = g->pac[base].orig;
    if (W)
        setk(g, base, orig + off);
    if (rt != base || W)
        gkill(g, rt);
    else
        gkill(g, rt);
}

/* DC ZVA, DC CVAU/CIVAC, IC IVAU and PRFM on data. */
static void
gen_cacheop(gen_t *g)
{
    asm_t *a = g->a;
    int t = (int)rn(&g->r, 5);
    if (t == 0 && !g->dczva_ok)
        t = 4;
    uint64_t addr;
    int fr = t == 0 ? fault_region(g, 64) : 0;
    if (fr == 2)
        fr = 1;
    if (!plan_addr(g, t == 0 ? 64 : 8, t == 0 ? 64 : 1, 1, fr, &addr))
        return;
    if (t == 0 && fr == 0 && addr >= STK_CMP_LO) {
        /* The whole 64-byte block must be writable stack above SP. */
        if ((addr & ~63ULL) < SPV)
            return;
    }
    /* Use an unaligned address inside the block sometimes. */
    if (t == 0 && rp(&g->r, 50))
        addr += rn(&g->r, 64);
    int base;
    int64_t off;
    if (t == 4) {
        if (!find_base(g, addr, F_U12, 8, 1, -1, -1, &base, &off))
            return;
        int at = emit(a, 0xf9800000u | U(off / 8) << 10 | U(base) << 5 | rn(&g->r, 32),
                      "prfm #x, [%s, #%lld]", XS(base), (long long)off);
        set_mi(g, at, base, off, 0, 0);
        set_acc(a, at, MA_PRFM, 0);
        g->dyn += g->mult;
        return;
    }
    if (!find_base(g, addr, F_ZERO, 1, 0, -1, -1, &base, &off))
        return;
    static const uint32_t ops[] = { 0xd50b7420u, 0xd50b7b20u, 0xd50b7e20u, 0xd50b7520u };
    static const char *nm[] = { "dc zva", "dc cvau", "dc civac", "ic ivau" };
    int at = emit(a, ops[t] | U(base), "%s, %s", nm[t], XR(base));
    set_mi(g, at, base, 0, 0, 0);
    set_acc(a, at, t == 3 ? MA_IFLUSH : MA_DFLUSH, 0);
    if (t == 0) {
        set_mi(g, at, base, 0, 0, 0);
        set_acc(a, at, MA_ZVA, 64);
        a->mi[at].zva = 1;
        if (fr)
            note_fault(g, at);
        else if (addr >= STK_CMP_LO)
            stk_written(g, addr & ~63ULL, 64);
    }
    g->dyn += g->mult;
}

/***************************************************************************
 * Integer data processing.
 */
static void
gen_alu(gen_t *g)
{
    asm_t *a = g->a;
    int sf = rp(&g->r, 70);
    int rd = pick_dst(g);
    if (rd < 0)
        return;
    int rn_ = pick_src(g), rm = pick_src(g);
    int t = (int)rn(&g->r, 20);
    uint32_t w;
    g->dyn += g->mult;
    switch (t) {
    case 0:
    case 1: {
        /* ADD/SUB(S) immediate; Rn may be SP. */
        int op = (int)rn(&g->r, 2), S = (int)rn(&g->r, 2), sh = rp(&g->r, 20);
        uint32_t imm = rn(&g->r, 4096);
        int rnx = rp(&g->r, 15) ? 31 : rn_;
        int rdx = S && rp(&g->r, 20) ? 31 : rd;
        w = e_addsub_imm(sf, op, S, sh, imm, rnx, rdx);
        emit(a, w, "%s%s %s, %s, #0x%x%s", op ? "sub" : "add", S ? "s" : "",
             S ? GR(sf, rdx) : GS(sf, rdx), GS(sf, rnx), imm, sh ? ", lsl #12" : "");
        if (rdx != 31) {
            if (g->known[rnx] && sf) {
                uint64_t v = g->val[rnx], im = (uint64_t)imm << (sh ? 12 : 0);
                setk(g, rdx, op ? v - im : v + im);
            } else
                gkill(g, rdx);
        }
        return;
    }
    case 2: {
        /* Logical immediate. */
        int opc = (int)rn(&g->r, 4);
        int lg = 1 + (int)rn(&g->r, sf ? 6 : 5);
        int esize = 1 << lg;
        int ones = 1 + (int)rn(&g->r, esize - 1);
        int immr = (int)rn(&g->r, esize);
        int N = esize == 64;
        int imms = ((0x3f << (lg + 1)) & 0x3f) | (ones - 1);
        int rdx = opc == 3 && rp(&g->r, 20) ? 31 : rd;
        emit(a, e_logic_imm(sf, opc, N, immr, imms, rn_, rdx),
             "%s %s, %s, #<N%d,r%d,s%d>",
             (const char *[]) { "and", "orr", "eor", "ands" }[opc], GR(sf, rdx),
             GR(sf, rn_), N, immr, imms);
        gkill(g, rdx);
        return;
    }
    case 3: {
        /* Logical shifted register. */
        int opc = (int)rn(&g->r, 4), N = (int)rn(&g->r, 2), sh = (int)rn(&g->r, 4);
        int imm6 = (int)rn(&g->r, sf ? 64 : 32);
        w = U(sf) << 31 | U(opc) << 29 | 0x0Au << 24 | U(sh) << 22 | U(N) << 21 |
            U(rm) << 16 | U(imm6) << 10 | U(rn_) << 5 | U(rd);
        emit(a, w, "%s %s, %s, %s, %s #%d",
             (const char *[]) { "and", "bic", "orr", "orn", "eor", "eon", "ands",
                                "bics" }[opc * 2 + N],
             GR(sf, rd), GR(sf, rn_), GR(sf, rm),
             (const char *[]) { "lsl", "lsr", "asr", "ror" }[sh], imm6);
        if (opc == 1 && N == 0 && rn_ == 31 && imm6 == 0 && sh == 0 && sf &&
            (rm == 31 || g->known[rm])) {
            /* mov; register 31 is XZR here, not SP (val[31]). */
            setk(g, rd, rm == 31 ? 0 : g->val[rm]);
        } else
            gkill(g, rd);
        return;
    }
    case 4: {
        /* ADD/SUB(S) shifted register. */
        int op = (int)rn(&g->r, 2), S = (int)rn(&g->r, 2), sh = (int)rn(&g->r, 3);
        int imm6 = (int)rn(&g->r, sf ? 64 : 32);
        int rdx = S && rp(&g->r, 20) ? 31 : rd;
        w = U(sf) << 31 | U(op) << 30 | U(S) << 29 | 0x0Bu << 24 | U(sh) << 22 |
            U(rm) << 16 | U(imm6) << 10 | U(rn_) << 5 | U(rdx);
        emit(a, w, "%s%s %s, %s, %s, %s #%d", op ? "sub" : "add", S ? "s" : "",
             GR(sf, rdx), GR(sf, rn_), GR(sf, rm),
             (const char *[]) { "lsl", "lsr", "asr" }[sh], imm6);
        gkill(g, rdx);
        return;
    }
    case 5: {
        /* ADD/SUB(S) extended register; Rn may be SP. */
        int op = (int)rn(&g->r, 2), S = (int)rn(&g->r, 2), opt = (int)rn(&g->r, 8);
        int imm3 = (int)rn(&g->r, 5);
        int rnx = rp(&g->r, 30) ? 31 : rn_;
        int rdx = S && rp(&g->r, 20) ? 31 : rd;
        w = U(sf) << 31 | U(op) << 30 | U(S) << 29 | 0x0Bu << 24 | 1u << 21 |
            U(rm) << 16 | U(opt) << 13 | U(imm3) << 10 | U(rnx) << 5 | U(rdx);
        emit(a, w, "%s%s %s, %s, %s, %s #%d", op ? "sub" : "add", S ? "s" : "",
             S ? GR(sf, rdx) : GS(sf, rdx), GS(sf, rnx), GR(sf && (opt & 3) == 3, rm),
             (const char *[]) { "uxtb", "uxth", "uxtw", "uxtx", "sxtb", "sxth", "sxtw",
                                "sxtx" }[opt],
             imm3);
        gkill(g, rdx);
        return;
    }
    case 6: {
        int op = (int)rn(&g->r, 2), S = (int)rn(&g->r, 2);
        w = U(sf) << 31 | U(op) << 30 | U(S) << 29 | 0xD0u << 21 | U(rm) << 16 |
            U(rn_) << 5 | U(rd);
        emit(a, w, "%s%s %s, %s, %s", op ? "sbc" : "adc", S ? "s" : "", GR(sf, rd),
             GR(sf, rn_), GR(sf, rm));
        gkill(g, rd);
        return;
    }
    case 7: {
        /* CCMP/CCMN register or immediate. */
        int op = (int)rn(&g->r, 2), imm = (int)rn(&g->r, 2), cond = (int)rn(&g->r, 16);
        int nzcv = (int)rn(&g->r, 16);
        int f = imm ? (int)rn(&g->r, 32) : rm;
        w = U(sf) << 31 | U(op) << 30 | 1u << 29 | 0xD2u << 21 | U(f) << 16 |
            U(cond) << 12 | U(imm) << 11 | U(rn_) << 5 | U(nzcv);
        if (imm) {
            emit(a, w, "%s %s, #%d, #%d, %s", op ? "ccmp" : "ccmn", GR(sf, rn_), f, nzcv,
                 cond_name[cond]);
        } else {
            emit(a, w, "%s %s, %s, #%d, %s", op ? "ccmp" : "ccmn", GR(sf, rn_), GR(sf, f),
                 nzcv, cond_name[cond]);
        }
        return;
    }
    case 8: {
        int op = (int)rn(&g->r, 2), op2 = (int)rn(&g->r, 2), cond = (int)rn(&g->r, 16);
        w = U(sf) << 31 | U(op) << 30 | 0xD4u << 21 | U(rm) << 16 | U(cond) << 12 |
            U(op2) << 10 | U(rn_) << 5 | U(rd);
        emit(a, w, "%s %s, %s, %s, %s",
             (const char *[]) { "csel", "csinc", "csinv", "csneg" }[op * 2 + op2],
             GR(sf, rd), GR(sf, rn_), GR(sf, rm), cond_name[cond]);
        gkill(g, rd);
        return;
    }
    case 9: {
        /* 2-source: div, shifts, crc32. */
        static const int ops[] = { 2, 3, 8, 9, 10, 11, 16, 17, 18, 19, 20, 21, 22, 23 };
        int opc = ops[rn(&g->r, feat.crc32 ? 14 : 6)];
        int s = sf;
        if (opc >= 16) {
            s = (opc & 3) == 3;
        }
        w = U(s) << 31 | 0xD6u << 21 | U(rm) << 16 | U(opc) << 10 | U(rn_) << 5 | U(rd);
        emit(a, w, "dp2[%d] %s, %s, %s", opc, GR(s, rd), opc >= 16 ? WR(rn_) : GR(s, rn_),
             GR(s, rm));
        gkill(g, rd);
        return;
    }
    case 10: {
        int opc = (int)rn(&g->r, 6);
        if (!sf && opc == 3)
            opc = 2;
        w = U(sf) << 31 | 1u << 30 | 0xD6u << 21 | U(opc) << 10 | U(rn_) << 5 | U(rd);
        emit(a, w, "%s %s, %s",
             (const char *[]) { "rbit", "rev16", sf ? "rev32" : "rev", "rev", "clz",
                                "cls" }[opc],
             GR(sf, rd), GR(sf, rn_));
        gkill(g, rd);
        return;
    }
    case 11: {
        static const int o31[] = { 0, 0, 1, 1, 2, 5, 5, 6 };
        static const int o0[] = { 0, 1, 0, 1, 0, 0, 1, 0 };
        static const char *nm[] = { "madd",  "msub",   "smaddl", "smsubl",
                                    "smulh", "umaddl", "umsubl", "umulh" };
        int t2 = (int)rn(&g->r, 8);
        int s = t2 >= 2 ? 1 : sf;
        int ra = (t2 == 4 || t2 == 7) ? 31 : pick_src(g);
        w = U(s) << 31 | 0x1Bu << 24 | U(o31[t2]) << 21 | U(rm) << 16 | U(o0[t2]) << 15 |
            U(ra) << 10 | U(rn_) << 5 | U(rd);
        emit(a, w, "%s %s, %s, %s, %s", nm[t2], XR(rd),
             t2 >= 2 && t2 != 4 && t2 != 7 ? WR(rn_) : GR(s, rn_),
             t2 >= 2 && t2 != 4 && t2 != 7 ? WR(rm) : GR(s, rm), GR(s, ra));
        gkill(g, rd);
        return;
    }
    case 12: {
        int opc = (int)rn(&g->r, 3);
        int immr = (int)rn(&g->r, sf ? 64 : 32), imms = (int)rn(&g->r, sf ? 64 : 32);
        emit(a, e_bitfield(sf, opc, immr, imms, rn_, rd), "%s %s, %s, #%d, #%d",
             (const char *[]) { "sbfm", "bfm", "ubfm" }[opc], GR(sf, rd), GR(sf, rn_),
             immr, imms);
        gkill(g, rd);
        return;
    }
    case 13: {
        int imms = (int)rn(&g->r, sf ? 64 : 32);
        w = U(sf) << 31 | 0x27u << 23 | U(sf) << 22 | U(rm) << 16 | U(imms) << 10 |
            U(rn_) << 5 | U(rd);
        emit(a, w, "extr %s, %s, %s, #%d", GR(sf, rd), GR(sf, rn_), GR(sf, rm), imms);
        gkill(g, rd);
        return;
    }
    case 14: {
        int opc = (int)rn(&g->r, 3);
        if (opc == 1)
            opc = 3; /* MOVK */
        int hw = (int)rn(&g->r, sf ? 4 : 2);
        uint32_t imm = U(rval(&g->r)) & 0xffff;
        emit(a, e_movw(sf, opc, hw, imm, rd), "%s %s, #0x%x, lsl #%d",
             (const char *[]) { "movn", "", "movz", "movk" }[opc], GR(sf, rd), imm,
             hw * 16);
        if (opc == 2 && sf)
            setk(g, rd, (uint64_t)imm << (16 * hw));
        else if (opc == 3 && sf && g->known[rd])
            setk(g, rd,
                 (g->val[rd] & ~(0xffffULL << (16 * hw))) | ((uint64_t)imm << (16 * hw)));
        else
            gkill(g, rd);
        return;
    }
    case 15: {
        if (!feat.flagm)
            return;
        int t2 = (int)rn(&g->r, 4);
        if (t2 == 0)
            emit(a, 0xd500401fu, "cfinv");
        else if (t2 == 1)
            emit(a, 0xba000400u | rn(&g->r, 64) << 15 | U(rn_) << 5 | rn(&g->r, 16),
                 "rmif %s, ...", XR(rn_));
        else
            emit(a, (t2 == 2 ? 0x3a00080du : 0x3a00480du) | U(rn_) << 5, "setf%d %s",
                 t2 == 2 ? 8 : 16, WR(rn_));
        return;
    }
    case 16: {
        /* ADR to somewhere in the block. */
        int64_t d = -(int64_t)rn(&g->r, (uint32_t)a->n) - 0;
        if (rp(&g->r, 30))
            d = rn(&g->r, 64);
        uint32_t imm = U(d * 4);
        emit(a, (imm & 3) << 29 | 0x10000000u | ((imm >> 2) & 0x7ffff) << 5 | U(rd),
             "adr %s, .%+lld", XR(rd), (long long)(d * 4));
        setk(g, rd, pc_of(a->n - 1) + (uint64_t)(d * 4));
        return;
    }
    case 17: {
        /* Pointer materialization (keeps memory ops plentiful). */
        uint64_t addr;
        if (plan_addr(g, 8, 8, 1, 0, &addr))
            mov_ptr(g, rd, addr);
        return;
    }
    case 18: {
        /* mov xd, sp / add xd, sp, #imm */
        uint32_t imm = rp(&g->r, 50) ? 0 : rn(&g->r, 256) * 16;
        emit(a, e_addsub_imm(1, 0, 0, 0, imm, 31, rd), "add %s, sp, #0x%x", XR(rd), imm);
        setk(g, rd, SPV + imm);
        return;
    }
    default: {
        /* SP adjustment within bounds (multiple of 16, never above the ceiling). */
        int64_t delta = ((int64_t)rn(&g->r, 33) - 16) * 16;
        uint64_t nsp = SPV + delta;
        if (nsp < SP_FLOOR || nsp > g->ceiling)
            return;
        if (rp(&g->r, 30) && delta != 0) {
            /* sub sp, sp, xm (extended register form). */
            int m = pick_dst(g);
            if (m < 0)
                return;
            mov_imm(g, m, (uint64_t)(delta < 0 ? -delta : delta));
            emit(a,
                 U(1) << 31 | U(delta < 0) << 30 | 0x0Bu << 24 | 1u << 21 | U(m) << 16 |
                     3u << 13 | 31u << 5 | 31u,
                 "%s sp, sp, %s", delta < 0 ? "sub" : "add", XR(m));
            uint64_t old = SPV;
            g->val[31] = nsp;
            sp_moved(g, old);
        } else
            sp_add(g, delta);
        return;
    }
    }
}

/***************************************************************************
 * FP and SIMD.
 */
static void
gen_fp(gen_t *g)
{
    asm_t *a = g->a;
    int vd = pick_v(g), vn = pick_v(g), vm = pick_v(g), va = pick_v(g);
    int ft = (int)rn(&g->r, 3);
    if (ft == 2)
        ft = feat.fp16 ? 3 : 0;
    int Q = (int)rn(&g->r, 2);
    int t = (int)rn(&g->r, 30);
    g->dyn += g->mult;
    switch (t) {
    case 0:
        emit(a,
             0x1E200800u | U(ft) << 22 | U(vm) << 16 | rn(&g->r, 9) << 12 | U(vn) << 5 |
                 U(vd),
             "fp2 t%d v%d, v%d, v%d", ft, vd, vn, vm);
        return;
    case 1: {
        static const int ops[] = { 0, 1, 2, 3, 8, 9, 10, 11, 12, 14, 15 };
        int op = ops[rn(&g->r, 11)];
        emit(a, 0x1E204000u | U(ft) << 22 | U(op) << 15 | U(vn) << 5 | U(vd),
             "fp1[%d] t%d v%d, v%d", op, ft, vd, vn);
        return;
    }
    case 2: {
        /* FCVT between precisions. */
        int to = (int)rn(&g->r, 3);
        int toenc = to == 2 ? 3 : to;
        if (toenc == ft || (!feat.fp16 && (toenc == 3 || ft == 3)))
            return;
        emit(a, 0x1E204000u | U(ft) << 22 | U(4 | (toenc & 3)) << 15 | U(vn) << 5 | U(vd),
             "fcvt t%d<-t%d v%d, v%d", toenc, ft, vd, vn);
        return;
    }
    case 3:
        emit(a,
             0x1F000000u | U(ft) << 22 | rn(&g->r, 2) << 21 | U(vm) << 16 |
                 rn(&g->r, 2) << 15 | U(va) << 10 | U(vn) << 5 | U(vd),
             "fmadd-family t%d v%d, v%d, v%d, v%d", ft, vd, vn, vm, va);
        return;
    case 4: {
        int opc = (int)rn(&g->r, 4);
        static const int enc[] = { 0, 0x10, 0x08, 0x18 };
        emit(a,
             0x1E202000u | U(ft) << 22 | U(opc >= 2 ? 0 : vm) << 16 | U(vn) << 5 |
                 U(enc[opc]),
             "fcmp%s t%d v%d, %s", opc & 1 ? "e" : "", ft, vn, opc >= 2 ? "#0" : "vm");
        return;
    }
    case 5:
        emit(a,
             0x1E200400u | U(ft) << 22 | U(vm) << 16 | rn(&g->r, 16) << 12 | U(vn) << 5 |
                 rn(&g->r, 2) << 4 | rn(&g->r, 16),
             "fccmp(e) t%d v%d, v%d", ft, vn, vm);
        return;
    case 6:
        emit(a,
             0x1E200C00u | U(ft) << 22 | U(vm) << 16 | rn(&g->r, 16) << 12 | U(vn) << 5 |
                 U(vd),
             "fcsel t%d v%d, v%d, v%d", ft, vd, vn, vm);
        return;
    case 7: {
        /* FMOV GPR <-> FP. */
        int k = (int)rn(&g->r, 6);
        static const uint32_t enc[] = { 0x1E260000u, 0x1E270000u, 0x9E660000u,
                                        0x9E670000u, 0x9EAE0000u, 0x9EAF0000u };
        int togpr = (k % 2) == 0;
        int r = togpr ? pick_dst(g) : pick_src(g);
        if (r < 0)
            return;
        if (togpr)
            emit(a, enc[k] | U(vn) << 5 | U(r), "fmov %s, v%d", GR(k >= 2, r), vn),
                gkill(g, r);
        else
            emit(a, enc[k] | U(r) << 5 | U(vd), "fmov v%d, %s", vd, GR(k >= 2, r));
        return;
    }
    case 8: {
        /* SCVTF/UCVTF from GPR. */
        int sf = (int)rn(&g->r, 2);
        int r = pick_src(g);
        emit(a,
             U(sf) << 31 | (rp(&g->r, 50) ? 0x1E220000u : 0x1E230000u) | U(ft) << 22 |
                 U(r) << 5 | U(vd),
             "s/ucvtf t%d v%d, %s", ft, vd, GR(sf, r));
        return;
    }
    case 9: {
        /* FP -> int conversions (and FJCVTZS which sets NZCV). */
        int sf = (int)rn(&g->r, 2);
        int r = pick_dst(g);
        if (r < 0)
            return;
        static const uint32_t enc[] = { 0x1E380000u, 0x1E390000u, 0x1E200000u,
                                        0x1E240000u, 0x1E300000u, 0x1E210000u };
        int k = (int)rn(&g->r, 7);
        if (k == 6) {
            if (!feat.jscvt)
                return;
            emit(a, 0x1E7E0000u | U(vn) << 5 | U(r), "fjcvtzs %s, d%d", WR(r), vn);
        } else
            emit(a, U(sf) << 31 | enc[k] | U(ft) << 22 | U(vn) << 5 | U(r),
                 "fcvt*[%d] %s, t%d v%d", k, GR(sf, r), ft, vn);
        gkill(g, r);
        return;
    }
    case 10:
        emit(a, 0x1E201000u | U(ft) << 22 | rn(&g->r, 256) << 13 | U(vd),
             "fmov t%d v%d, #imm", ft, vd);
        return;
    case 11:
    case 12: {
        /* Integer 3-same. */
        static const struct {
            int U, opc, size; /* size -1: any (not 3 unless Q) */
        } ops[] = { { 0, 16, -1 }, { 1, 16, -1 }, { 0, 19, -2 }, { 0, 3, 0 },
                    { 0, 3, 1 },   { 0, 3, 2 },   { 0, 3, 3 },   { 1, 3, 0 },
                    { 1, 3, 1 },   { 1, 3, 2 },   { 1, 3, 3 },   { 0, 1, -1 },
                    { 1, 1, -1 },  { 0, 5, -1 },  { 1, 17, -1 }, { 0, 6, -1 },
                    { 1, 6, -1 },  { 0, 8, -1 },  { 1, 8, -1 },  { 0, 20, -2 } };
        int k = (int)rn(&g->r, 20);
        int size = ops[k].size;
        if (size == -1) {
            size = (int)rn(&g->r, 4);
            if (size == 3 && !Q)
                size = 2;
        } else if (size == -2)
            size = (int)rn(&g->r, 3);
        emit(a,
             0x0E200400u | U(Q) << 30 | U(ops[k].U) << 29 | U(size) << 22 | U(vm) << 16 |
                 U(ops[k].opc) << 11 | U(vn) << 5 | U(vd),
             "simd3[U%d,%d,sz%d] v%d.%s, v%d, v%d", ops[k].U, ops[k].opc, size, vd,
             Q ? "q" : "d", vn, vm);
        return;
    }
    case 13: {
        /* FP 3-same vector. */
        static const struct {
            int U, opc, hi;
        } ops[] = { { 0, 26, 0 }, { 1, 27, 0 }, { 0, 25, 0 }, { 0, 30, 0 },
                    { 0, 28, 0 }, { 1, 28, 0 }, { 0, 31, 0 }, { 1, 31, 0 },
                    { 0, 26, 1 }, { 1, 26, 0 }, { 0, 30, 1 }, { 1, 30, 0 } };
        int k = (int)rn(&g->r, 12);
        int sz = Q ? (int)rn(&g->r, 2) : 0;
        emit(a,
             0x0E200400u | U(Q) << 30 | U(ops[k].U) << 29 | U(ops[k].hi) << 23 |
                 U(sz) << 22 | U(vm) << 16 | U(ops[k].opc) << 11 | U(vn) << 5 | U(vd),
             "fsimd3[U%d,%d,%d] v%d, v%d, v%d", ops[k].U, ops[k].opc, ops[k].hi, vd, vn,
             vm);
        return;
    }
    case 14: {
        /* 2-reg misc.  size: -1 int (no .1d), -2 int <= 32-bit, -3 FP size<1>=0,
         * -4 FP size<1>=1, >= 0 fixed.
         */
        static const struct {
            int U, opc, size;
        } ops[] = { { 0, 5, 0 },   { 0, 0, -2 },  { 1, 5, 0 },   { 0, 11, -1 },
                    { 1, 11, -1 }, { 0, 7, -1 },  { 0, 4, -2 },  { 1, 4, -2 },
                    { 0, 29, -4 }, { 1, 31, -4 }, { 0, 27, -4 }, { 0, 29, -3 },
                    { 1, 29, -4 }, { 0, 24, -3 }, { 1, 25, -4 } };
        int k = (int)rn(&g->r, 15);
        int size = ops[k].size;
        if (size == -1) {
            size = (int)rn(&g->r, 4);
            if (size == 3 && !Q)
                size = 2;
        } else if (size == -2)
            size = (int)rn(&g->r, 3);
        else if (size == -3 || size == -4)
            size = (size == -4 ? 2 : 0) | (Q ? (int)rn(&g->r, 2) : 0);
        emit(a,
             0x0E200800u | U(Q) << 30 | U(ops[k].U) << 29 | U(size) << 22 |
                 U(ops[k].opc) << 12 | U(vn) << 5 | U(vd),
             "simd2misc[U%d,%d,sz%d] v%d, v%d", ops[k].U, ops[k].opc, size, vd, vn);
        return;
    }
    case 15: {
        int size = (int)rn(&g->r, 3);
        if (size == 2 && !Q)
            size = 1;
        int k = (int)rn(&g->r, 3);
        static const uint32_t enc[] = { 0x0E31B800u, 0x2E30A800u, 0x0E30A800u };
        emit(a, enc[k] | U(Q) << 30 | U(size) << 22 | U(vn) << 5 | U(vd),
             "across[%d] v%d, v%d", k, vd, vn);
        return;
    }
    case 16: {
        /* DUP element / DUP general / INS general / UMOV / INS element. */
        int esz = (int)rn(&g->r, 4);
        int idx = (int)rn(&g->r, 16 >> esz);
        uint32_t imm5 = (U(idx) << (esz + 1)) | (1u << esz);
        int k = (int)rn(&g->r, 5);
        if (k == 0)
            emit(a,
                 0x0E000400u | U(esz == 3 ? 1 : Q) << 30 | imm5 << 16 | U(vn) << 5 |
                     U(vd),
                 "dup v%d, v%d[%d]", vd, vn, idx);
        else if (k == 1) {
            int r = pick_src(g);
            emit(a,
                 0x0E000C00u | U(esz == 3 ? 1 : Q) << 30 | imm5 << 16 | U(r) << 5 | U(vd),
                 "dup v%d, %s", vd, XR(r));
        } else if (k == 2) {
            int r = pick_src(g);
            emit(a, 0x4E001C00u | imm5 << 16 | U(r) << 5 | U(vd), "ins v%d[%d], %s", vd,
                 idx, XR(r));
        } else if (k == 3) {
            int r = pick_dst(g);
            if (r < 0)
                return;
            emit(a, 0x0E003C00u | U(esz == 3) << 30 | imm5 << 16 | U(vn) << 5 | U(r),
                 "umov %s, v%d[%d]", GR(esz == 3, r), vn, idx);
            gkill(g, r);
        } else {
            int idx2 = (int)rn(&g->r, 16 >> esz);
            emit(a,
                 0x6E000400u | imm5 << 16 | (U(idx2) << esz) << 11 | U(vn) << 5 | U(vd),
                 "ins v%d[%d], v%d[%d]", vd, idx, vn, idx2);
        }
        return;
    }
    case 17:
        emit(a,
             0x2E000000u | U(Q) << 30 | U(vm) << 16 | rn(&g->r, Q ? 16 : 8) << 11 |
                 U(vn) << 5 | U(vd),
             "ext v%d, v%d, v%d, #i", vd, vn, vm);
        return;
    case 18: {
        int len = (int)rn(&g->r, 4);
        for (int i = 0; i <= len; i++) {
            if (((vn + i) & 31) == g->vres)
                return;
        }
        emit(a,
             0x0E000000u | U(Q) << 30 | U(vm) << 16 | U(len) << 13 | rn(&g->r, 2) << 12 |
                 U(vn) << 5 | U(vd),
             "tbl/tbx v%d, {v%d +%d}, v%d", vd, vn, len, vm);
        return;
    }
    case 19: {
        static const int ops[] = { 1, 2, 3, 5, 6, 7 };
        int size = (int)rn(&g->r, 4);
        if (size == 3 && !Q)
            size = 2;
        emit(a,
             0x0E000800u | U(Q) << 30 | U(size) << 22 | U(vm) << 16 |
                 U(ops[rn(&g->r, 6)]) << 12 | U(vn) << 5 | U(vd),
             "zip/uzp/trn v%d, v%d, v%d", vd, vn, vm);
        return;
    }
    case 20: {
        /* Shift by immediate: SHL, USHR, SSHR, SRI, SQSHL. */
        int k = (int)rn(&g->r, 3);
        int esz = 8 << rn(&g->r, 4);
        if (esz == 64)
            Q = 1;
        int sh;
        uint32_t immhb;
        if (k == 0) {
            sh = (int)rn(&g->r, esz);
            immhb = U(esz + sh);
            emit(a, 0x0F005400u | U(Q) << 30 | immhb << 16 | U(vn) << 5 | U(vd),
                 "shl v%d, v%d, #%d", vd, vn, sh);
        } else {
            sh = 1 + (int)rn(&g->r, esz);
            immhb = U(2 * esz - sh);
            emit(a,
                 (k == 1 ? 0x2F000400u : 0x0F000400u) | U(Q) << 30 | immhb << 16 |
                     U(vn) << 5 | U(vd),
                 "%s v%d, v%d, #%d", k == 1 ? "ushr" : "sshr", vd, vn, sh);
        }
        return;
    }
    case 21:
        if (!feat.aes)
            return;
        emit(a,
             (const uint32_t[]) { 0x4E284800u, 0x4E285800u, 0x4E286800u,
                                  0x4E287800u }[rn(&g->r, 4)] |
                 U(vn) << 5 | U(vd),
             "aes* v%d, v%d", vd, vn);
        return;
    case 22:
        if (!feat.dotprod)
            return;
        emit(a,
             0x0E809400u | U(Q) << 30 | rn(&g->r, 2) << 29 | U(vm) << 16 | U(vn) << 5 |
                 U(vd),
             "s/udot v%d, v%d, v%d", vd, vn, vm);
        return;
    case 23:
        if (!feat.rdm)
            return;
        {
            int size = 1 + (int)rn(&g->r, 2);
            emit(a,
                 0x2E008400u | U(Q) << 30 | U(size) << 22 | U(vm) << 16 |
                     rn(&g->r, 2) << 11 | U(vn) << 5 | U(vd),
                 "sqrdmlah/sh v%d, v%d, v%d", vd, vn, vm);
        }
        return;
    case 24:
        if (!feat.fcma)
            return;
        {
            int size = Q ? 1 + (int)rn(&g->r, 3) : 1 + (int)rn(&g->r, 2);
            if (!feat.fp16 && size == 1)
                size = 2;
            emit(a,
                 0x2E00C400u | U(Q) << 30 | U(size) << 22 | U(vm) << 16 |
                     rn(&g->r, 4) << 11 | U(vn) << 5 | U(vd),
                 "fcmla v%d, v%d, v%d, #rot", vd, vn, vm);
        }
        return;
    case 25:
        if (!feat.i8mm)
            return;
        emit(a, 0x4E80A400u | rn(&g->r, 2) << 29 | U(vm) << 16 | U(vn) << 5 | U(vd),
             "smmla/ummla v%d", vd);
        return;
    case 26:
        if (!feat.bf16)
            return;
        emit(a, 0x2E40FC00u | U(Q) << 30 | U(vm) << 16 | U(vn) << 5 | U(vd),
             "bfdot v%d, v%d, v%d", vd, vn, vm);
        return;
    case 27:
        if (!feat.frint)
            return;
        emit(a, 0x1E284000u | U(ft & 1) << 22 | rn(&g->r, 4) << 15 | U(vn) << 5 | U(vd),
             "frint32/64x/z v%d, v%d", vd, vn);
        return;
    default:
        /* Vector FP compare to zero / FRINT vector. */
        emit(a, 0x4EA0D800u | rn(&g->r, 2) << 22 | U(vn) << 5 | U(vd), "frecpe v%d, v%d",
             vd, vn);
        return;
    }
}

/***************************************************************************
 * System registers, barriers, hints, syscalls.
 */
static uint64_t
rand_fpcr(gen_t *g)
{
    uint64_t v = 0;
    v |= (uint64_t)rn(&g->r, 4) << 22; /* RMode */
    if (rp(&g->r, 30))
        v |= 1ULL << 24; /* FZ */
    if (rp(&g->r, 30))
        v |= 1ULL << 25; /* DN */
    if (rp(&g->r, 30))
        v |= 1ULL << 26; /* AHP */
    if (feat.fp16 && rp(&g->r, 30))
        v |= 1ULL << 19; /* FZ16 */
    if (feat.afp) {
        if (rp(&g->r, 20))
            v |= 1ULL << 0; /* FIZ */
        if (rp(&g->r, 20))
            v |= 1ULL << 1; /* AH */
        if (rp(&g->r, 20))
            v |= 1ULL << 2; /* NEP */
    }
    return v;
}

static uint64_t
rand_fpsr(gen_t *g)
{
    static const int bits[] = { 0, 1, 2, 3, 4, 7, 27 };
    uint64_t v = 0;
    for (int i = 0; i < 7; i++) {
        if (rp(&g->r, 25))
            v |= 1ULL << bits[i];
    }
    return v;
}

static void
gen_sys(gen_t *g)
{
    asm_t *a = g->a;
    int t = (int)rn(&g->r, 14);
    g->dyn += g->mult;
    if (t <= 6) {
        /* MRS reads. */
        static const uint32_t regs[] = { SR_NZCV,    SR_FPCR, SR_FPSR,  SR_TPIDR,
                                         SR_TPIDRRO, SR_CTR,  SR_DCZID, SR_MIDR,
                                         SR_ISAR0,   SR_PFR0, SR_DIT };
        static const char *nm[] = {
            "nzcv",    "fpcr",      "fpsr",     "tpidr_el0",        "tpidrro_el0",
            "ctr_el0", "dczid_el0", "midr_el1", "id_aa64isar0_el1", "id_aa64pfr0_el1",
            "dit"
        };
        int k = (int)rn(&g->r, 11);
        if (k == 5 && !g->allow_ctr)
            k = 0;
        if (k == 10 && !feat.dit)
            k = 0;
        int rt = rp(&g->r, 5) ? 31 : pick_dst(g);
        if (rt < 0)
            return;
        emit(a, e_mrs(regs[k], rt), "mrs %s, %s", XR(rt), nm[k]);
        gkill(g, rt);
        return;
    }
    if (t == 7) {
        int rs = pick_src(g);
        emit(a, e_msr(SR_NZCV, rs), "msr nzcv, %s", XR(rs));
        return;
    }
    if (t == 8 && g->allow_fpcr) {
        int rs = pick_dst(g);
        if (rs < 0)
            return;
        int fpcr = rp(&g->r, 50);
        mov_imm(g, rs, fpcr ? rand_fpcr(g) : rand_fpsr(g));
        emit(a, e_msr(fpcr ? SR_FPCR : SR_FPSR, rs), "msr %s, %s", fpcr ? "fpcr" : "fpsr",
             XR(rs));
        return;
    }
    if (t == 9 && feat.dit) {
        if (rp(&g->r, 50))
            emit(a, 0xd503405fu | rn(&g->r, 2) << 8, "msr dit, #i");
        else {
            int rs = pick_dst(g);
            if (rs < 0)
                return;
            mov_imm(g, rs, rp(&g->r, 50) ? 1ULL << 24 : 0);
            emit(a, e_msr(SR_DIT, rs), "msr dit, %s", XR(rs));
        }
        return;
    }
    if (t == 10) {
        static const uint32_t enc[] = { 0xd5033bbfu, 0xd5033b9fu, 0xd5033fdfu,
                                        0xd503305fu, 0xd503201fu, 0xd503241fu,
                                        0xd503245fu, 0xd503229fu, 0xd5033f9fu,
                                        0xd50330ffu };
        static const char *nm[] = { "dmb ish", "dsb ish", "isb",  "clrex",  "nop",
                                    "bti",     "bti c",   "csdb", "dsb sy", "sb" };
        int k = (int)rn(&g->r, 10);
        if (k == 9 && !feat.sb)
            k = 4;
        emit(a, enc[k], "%s", nm[k]);
        return;
    }
    if (t == 11 && g->allow_svc) {
        /* Syscalls with deterministic results. */
        static const int nrs[] = { 174, 176, 124, 64, 57, 4000 };
        int k = (int)rn(&g->r, 6);
        if (g->lock[0] || g->lock[8] || !usable(g, 0) || !usable(g, 8))
            return;
        if (k == 3 || k == 4)
            mov_imm(g, 0, ~0ULL); /* fd -1 */
        if (k == 3) {
            if (g->lock[2] || !usable(g, 2))
                return;
            mov_imm(g, 2, 0);
        }
        mov_imm(g, 8, (uint64_t)nrs[k]);
        emit(a, 0xd4000001u, "svc #0 (nr %d)", nrs[k]);
        gkill(g, 0);
        return;
    }
    if (t == 12 && fault_budget(g) && rp(&g->r, 50)) {
        int at;
        if (g->allow_brk && rp(&g->r, 50))
            at = emit(a, 0xd4200000u | rn(&g->r, 0x10000) << 5, "brk #i");
        else
            at = emit(a, rn(&g->r, 0x10000), "udf #i");
        note_fault(g, at);
        return;
    }
    if (t == 13 && fault_budget(g) && rp(&g->r, 30)) {
        /* SP alignment fault: misalign SP, use it as a base, fix it up. */
        int rt = pick_dst(g);
        if (rt < 0 || SPV - 8 < SP_FLOOR)
            return;
        emit(a, e_addsub_imm(1, 1, 0, 0, 8, 31, 31), "sub sp, sp, #8");
        int at = emit(a, e_ldst_uimm(3, 0, 1, 1, 31, rt),
                      "ldr %s, [sp, #8] (sp misaligned)", XR(rt));
        set_mi(g, at, 31, 8, 0, 1);
        set_acc(a, at, MA_R, 8);
        note_fault(g, at);
        gkill(g, rt);
        emit(a, e_addsub_imm(1, 0, 0, 0, 8, 31, 31), "add sp, sp, #8");
        return;
    }
}

/***************************************************************************
 * Pointer authentication (data processing).
 */
static int
mod_source(gen_t *g, uint64_t mod, int avoid, int *zform)
{
    /* Returns a register (31 == SP) holding mod, or -1; *zform for zero. */
    *zform = 0;
    if (mod == 0 && rp(&g->r, 60)) {
        *zform = 1;
        return -1;
    }
    if (SPV == mod && rp(&g->r, 70))
        return 31;
    for (int r = 0; r < 31; r++) {
        if (r != avoid && g->known[r] && g->val[r] == mod && usable(g, r))
            return r;
    }
    if (mod == 0) {
        *zform = 1;
        return -1;
    }
    return -2;
}

static void
gen_pac(gen_t *g)
{
    asm_t *a = g->a;
    if (!feat.pauth)
        return;
    g->dyn += g->mult;
    int t = (int)rn(&g->r, 6);
    if (t <= 1) {
        /* Sign a known register. */
        int p = pick_dst(g);
        /* Only canonical user addresses: signing anything else yields a value
         * that is guaranteed to fail authentication.
         */
        if (p < 0 || !g->known[p] || g->val[p] >> 48 != 0)
            return;
        int key = (int)rn(&g->r, 4);
        int z = rp(&g->r, 25);
        int m = 31;
        uint64_t mod = 0;
        if (!z) {
            m = rp(&g->r, 30) ? 31 : pick_src(g);
            if (m != 31 && (!g->known[m] || m == p)) {
                m = 31;
            }
            mod = g->val[m]; /* val[31] is SP */
        }
        uint64_t orig = g->val[p];
        if (!z && p == 30 && m == 31 && key < 2 && rp(&g->r, 60)) {
            emit(a, key ? 0xd503237fu : 0xd503233fu, "paci%csp", key ? 'b' : 'a');
        } else if (z && p == 30 && key < 2 && rp(&g->r, 60)) {
            emit(a, key ? 0xd503235fu : 0xd503231fu, "paci%cz", key ? 'b' : 'a');
        } else if (!z && p == 17 && m == 16 && key < 2) {
            emit(a, key ? 0xd503215fu : 0xd503211fu, "paci%c1716", key ? 'b' : 'a');
        } else {
            static const char *kn[] = { "ia", "ib", "da", "db" };
            emit(a, 0xDAC10000u | U((z ? 8 : 0) | key) << 10 | U(z ? 31 : m) << 5 | U(p),
                 "pac%s%s %s%s%s", z ? "z" : "", kn[key], XR(p), z ? "" : ", ",
                 z ? "" : XS(m));
        }
        g->known[p] = 0;
        g->pac[p].st = 1;
        g->pac[p].key = key;
        g->pac[p].mod = mod;
        g->pac[p].orig = orig;
        return;
    }
    /* Find a signed register. */
    int cand[31], nc = 0;
    for (int r = 0; r < 31; r++) {
        if (g->pac[r].st == 1 && !g->lock[r] && usable(g, r))
            cand[nc++] = r;
    }
    if (nc == 0)
        return;
    int p = cand[rn(&g->r, nc)];
    pacst_t *ps = &g->pac[p];
    static const char *kn[] = { "ia", "ib", "da", "db" };
    if (t == 2) {
        /* Strip. */
        int d = ps->key >= 2;
        emit(a, 0xDAC10000u | U(16 + d) << 10 | 31u << 5 | U(p), "xpac%c %s",
             d ? 'd' : 'i', XR(p));
        setk(g, p, ps->orig);
        return;
    }
    if (t == 3 && feat.pacg) {
        /* PACGA (needs the generic key: HWCAP_PACG, not just HWCAP_PACA) */
        int d = pick_dst(g), m = rp(&g->r, 30) ? 31 : pick_src(g);
        if (d < 0)
            return;
        int s = pick_src(g);
        emit(a, 0x9AC03000u | U(m) << 16 | U(s) << 5 | U(d), "pacga %s, %s, %s", XR(d),
             XR(s), XS(m));
        gkill(g, d);
        return;
    }
    /* Authenticate, correctly (or deliberately wrongly when faults are allowed). */
    int wrong = fault_budget(g) && rp(&g->r, 15);
    int z;
    int m = mod_source(g, ps->mod, p, &z);
    if (m == -2) {
        int r = pick_dst(g);
        if (r < 0 || r == p)
            return;
        mov_imm(g, r, ps->mod);
        m = r;
        z = 0;
    }
    if (wrong) {
        if (z) {
            z = 0;
            m = 31;
            if (SPV == ps->mod)
                return;
        } else if (m == 31 ? SPV != 0 : 1) {
            z = 1;
            if (ps->mod == 0)
                return;
        } else
            return;
    }
    if (!z && p == 30 && m == 31 && ps->key < 2 && rp(&g->r, 60)) {
        emit(a, ps->key ? 0xd50323ffu : 0xd50323bfu, "auti%csp", ps->key ? 'b' : 'a');
    } else if (z && p == 30 && ps->key < 2 && rp(&g->r, 60)) {
        emit(a, ps->key ? 0xd50323dfu : 0xd503239fu, "auti%cz", ps->key ? 'b' : 'a');
    } else if (!z && p == 17 && m == 16 && ps->key < 2) {
        emit(a, ps->key ? 0xd50321dfu : 0xd503219fu, "auti%c1716", ps->key ? 'b' : 'a');
    } else {
        emit(a, 0xDAC10000u | U((z ? 12 : 4) | ps->key) << 10 | U(z ? 31 : m) << 5 | U(p),
             "aut%s%s %s%s%s", z ? "z" : "", kn[ps->key], XR(p), z ? "" : ", ",
             z ? "" : XS(m));
    }
    if (!wrong) {
        setk(g, p, ps->orig);
        return;
    }
    note_fault(g, a->n - 1); /* FEAT_FPAC faults at the AUT itself */
    /* Corrupted pointer: optionally dereference it (planned fault). */
    uint64_t orig = ps->orig;
    gkill(g, p);
    if (in_d(orig, 8) && rp(&g->r, 70)) {
        int rt = pick_dst(g);
        if (rt < 0)
            return;
        int at = emit(a, e_ldst_uimm(3, 0, 1, 0, p, rt), "ldr %s, [%s] (auth failed)",
                      XR(rt), XR(p));
        set_mi(g, at, p, 0, 0, 1);
        set_acc(a, at, MA_R, 8);
        note_fault(g, at);
        gkill(g, rt);
    }
}

/***************************************************************************
 * Control flow.
 */
static void
gen_seq(gen_t *g, int len);

static int
budget_ok(gen_t *g, uint64_t extra_mult)
{
    return g->dyn + g->mult * extra_mult * 24 < g->dyn_max;
}

static void
gen_loop(gen_t *g)
{
    asm_t *a = g->a;
    int c = pick_dst(g);
    if (c < 0)
        return;
    uint32_t n = 1 + rn(&g->r, 6);
    if (g->depth == 0 && g->dyn_max > 4000)
        n = 2 + rn(&g->r, 40);
    while (n > 1 && !budget_ok(g, n))
        n /= 2;
    if (!budget_ok(g, n))
        return;
    int style = (int)rn(&g->r, 4);
    if (style == 3)
        mov_imm(g, c, 0);
    else
        mov_imm(g, c, n);
    g->lock[c]++;
    forget_pac(g);
    /* The counter's value differs per iteration: unknown inside the body. */
    gkill(g, c);
    snap_t *s = malloc(sizeof(*s));
    uint64_t saved_ceiling = g->ceiling;
    g->ceiling = SPV;
    snap(g, s);
    int head = lab_new(a);
    lab_bind(a, head);
    uint64_t saved_mult = g->mult;
    g->mult *= n;
    g->depth++;
    gen_seq(g, 2 + (int)rn(&g->r, 10));
    g->depth--;
    restore_to(g, s);
    int at;
    switch (style) {
    case 0:
        emit(a, e_addsub_imm(1, 1, 1, 0, 1, c, c), "subs %s, %s, #1", XR(c), XR(c));
        at = emit(a, 0x54000000u | 1u, "b.ne L%d", head);
        fixup(a, at, head, FX_B19);
        break;
    case 1:
        emit(a, e_addsub_imm(1, 1, 0, 0, 1, c, c), "sub %s, %s, #1", XR(c), XR(c));
        at = emit(a, 0xb5000000u | U(c), "cbnz %s, L%d", XR(c), head);
        fixup(a, at, head, FX_B19);
        break;
    case 2:
        emit(a, e_addsub_imm(1, 1, 1, 0, 1, c, c), "subs %s, %s, #1", XR(c), XR(c));
        at = emit(a, 0x54000000u | 0xcu, "b.gt L%d", head);
        fixup(a, at, head, FX_B19);
        break;
    default:
        emit(a, e_addsub_imm(1, 0, 0, 0, 1, c, c), "add %s, %s, #1", XR(c), XR(c));
        emit(a, e_addsub_imm(1, 1, 1, 0, n, c, 31), "cmp %s, #%u", XR(c), n);
        at = emit(a, 0x54000000u | 0xbu, "b.lt L%d", head);
        fixup(a, at, head, FX_B19);
        break;
    }
    g->mult = saved_mult;
    g->lock[c]--;
    g->ceiling = saved_ceiling;
    setk(g, c, style == 3 ? n : 0);
    g->dyn += 3 * g->mult * n;
    free(s);
}

static void
gen_if(gen_t *g)
{
    asm_t *a = g->a;
    int skip = lab_new(a);
    int k = (int)rn(&g->r, 3), at;
    if (k == 0) {
        int cond = (int)rn(&g->r, 15);
        at = emit(a, 0x54000000u | U(cond), "b.%s L%d", cond_name[cond], skip);
        fixup(a, at, skip, FX_B19);
    } else if (k == 1) {
        int r = pick_src(g), sf = (int)rn(&g->r, 2), nz = (int)rn(&g->r, 2);
        at = emit(a, U(sf) << 31 | 0x34000000u | U(nz) << 24 | U(r), "cb%sz %s, L%d",
                  nz ? "n" : "", GR(sf, r), skip);
        fixup(a, at, skip, FX_B19);
    } else {
        int r = pick_src(g), bit = (int)rn(&g->r, 64), nz = (int)rn(&g->r, 2);
        at = emit(
            a, U(bit >> 5) << 31 | 0x36000000u | U(nz) << 24 | U(bit & 31) << 19 | U(r),
            "tb%sz %s, #%d, L%d", nz ? "n" : "", XR(r), bit, skip);
        fixup(a, at, skip, FX_B14);
    }
    forget_pac(g);
    snap_t *s = malloc(sizeof(*s));
    uint64_t saved_ceiling = g->ceiling;
    g->ceiling = SPV;
    snap(g, s);
    g->depth++;
    gen_seq(g, 1 + (int)rn(&g->r, 8));
    g->depth--;
    restore_to(g, s);
    /* Bytes written only in the body are not definitely initialized. */
    memcpy(g->sinit, s->sinit, sizeof(g->sinit));
    g->ceiling = saved_ceiling;
    lab_bind(a, skip);
    free(s);
}

/* A subroutine called by BL/BLR/BLRAA, placed out of line. */
static void
gen_call(gen_t *g)
{
    asm_t *a = g->a;
    if (g->lock[30] || !usable(g, 30) || g->depth > 2)
        return;
    forget_pac(g);
    int sub = lab_new(a), over = lab_new(a);
    int kind = (int)rn(&g->r, 4); /* 0 bl, 1 blr, 2 blraa/ab, 3 blraaz/abz */
    if (!feat.pauth && kind >= 2)
        kind = 1;
    int t = -1, mreg = -1, key = (int)rn(&g->r, 2);
    if (kind >= 1) {
        t = pick_dst(g);
        if (t < 0)
            return;
        int at = emit(a, 0x10000000u | U(t), "adr %s, L%d", XR(t), sub);
        fixup(a, at, sub, FX_ADR);
        gkill(g, t);
        if (kind == 2) {
            mreg = rp(&g->r, 30) ? 31 : pick_src(g);
            if (mreg == t)
                mreg = 31;
            emit(a, 0xDAC10000u | U(key) << 10 | U(mreg) << 5 | U(t), "paci%c %s, %s",
                 key ? 'b' : 'a', XR(t), XS(mreg));
        } else if (kind == 3)
            emit(a, 0xDAC10000u | U(8 + key) << 10 | 31u << 5 | U(t), "paciz%c %s",
                 key ? 'b' : 'a', XR(t));
    }
    int at;
    if (kind == 0) {
        at = emit(a, 0x94000000u, "bl L%d", sub);
        fixup(a, at, sub, FX_B26);
    } else if (kind == 1)
        emit(a, 0xd63f0000u | U(t) << 5, "blr %s", XR(t));
    else if (kind == 2)
        emit(a, 0xd73f0800u | U(key) << 10 | U(t) << 5 | U(mreg), "blra%c %s, %s",
             key ? 'b' : 'a', XR(t), XS(mreg));
    else
        emit(a, 0xd63f081fu | U(key) << 10 | U(t) << 5, "blra%cz %s", key ? 'b' : 'a',
             XR(t));
    int ret_idx = a->n;
    /* After return: x30 holds the return address; the subroutine restores the rest. */
    snap_t *s = malloc(sizeof(*s));
    setk(g, 30, pc_of(ret_idx));
    if (t >= 0)
        gkill(g, t);
    snap(g, s);
    at = emit(a, 0x14000000u, "b L%d", over);
    fixup(a, at, over, FX_B26);
    /* Subroutine body. */
    lab_bind(a, sub);
    if (kind >= 1 && rp(&g->r, 30))
        emit(a, 0xd503245fu, "bti c");
    uint64_t saved_ceiling = g->ceiling;
    g->ceiling = SPV;
    int ret = (int)rn(
        &g->r, 5); /* 0 ret, 1 retaa/ab (signed), 2 autiasp+ret, 3 ret xN, 4 framed */
    if (!feat.pauth && (ret == 1 || ret == 2))
        ret = 0;
    int rkey = (int)rn(&g->r, 2);
    if (ret == 1 || ret == 2) {
        emit(a, rkey ? 0xd503237fu : 0xd503233fu, "paci%csp", rkey ? 'b' : 'a');
        gkill(g, 30);
    }
    int framed = ret == 4;
    uint64_t saved_plo = g->prot_lo, saved_phi = g->prot_hi;
    uint64_t sp_at_entry = SPV;
    if (framed) {
        if (SPV - 16 < SP_FLOOR) {
            framed = 0;
            ret = 0;
        } else {
            int at2 = emit(a, e_ldst_pair(2, 0, 3, 0, -2, 30, 31, 29),
                           "stp x29, x30, [sp, #-16]!");
            set_mi(g, at2, 31, -16, 1, 1);
            set_acc(a, at2, MA_W, 16);
            uint64_t old = SPV;
            g->val[31] -= 16;
            (void)old;
            stk_written(g, SPV, 16);
            g->ceiling = SPV;
            /* Keep protecting the frames of enclosing calls: they are above
             * this one.
             */
            g->prot_lo = SPV;
            if (saved_phi <= saved_plo || saved_phi < SPV + 16)
                g->prot_hi = SPV + 16;
        }
    }
    snap_t *s2 = malloc(sizeof(*s2));
    snap(g, s2);
    if (!framed)
        g->lock[30]++;
    g->depth++;
    gen_seq(g, 1 + (int)rn(&g->r, 8));
    g->depth--;
    if (!framed)
        g->lock[30]--;
    restore_to(g, s2);
    if (framed) {
        int at2 =
            emit(a, e_ldst_pair(2, 0, 1, 1, 2, 30, 31, 29), "ldp x29, x30, [sp], #16");
        set_mi(g, at2, 31, 0, 2, 1);
        set_acc(a, at2, MA_R, 16);
        uint64_t old = SPV;
        g->val[31] += 16;
        sp_moved(g, old);
    }
    (void)sp_at_entry;
    g->prot_lo = saved_plo;
    g->prot_hi = saved_phi;
    int retreg = -1;
    if (ret == 1)
        emit(a, rkey ? 0xd65f0fffu : 0xd65f0bffu, "reta%c", rkey ? 'b' : 'a');
    else if (ret == 2) {
        emit(a, rkey ? 0xd50323ffu : 0xd50323bfu, "auti%csp", rkey ? 'b' : 'a');
        emit(a, 0xd65f03c0u, "ret");
    } else if (ret == 3) {
        int r = pick_dst(g);
        if (r < 0 || r == 30)
            r = 30;
        if (r != 30)
            emit(a, U(1) << 31 | U(1) << 29 | 0x0Au << 24 | 30u << 16 | 31u << 5 | U(r),
                 "mov %s, x30", XR(r));
        emit(a, 0xd65f0000u | U(r) << 5, "ret %s", XR(r));
        retreg = r;
    } else
        emit(a, 0xd65f03c0u, "ret");
    g->ceiling = saved_ceiling;
    lab_bind(a, over);
    /* Continue with the call-site state (sub preserved all known regs). */
    memcpy(g->known, s->known, sizeof(g->known));
    memcpy(g->val, s->val, sizeof(g->val));
    memcpy(g->pac, s->pac, sizeof(g->pac));
    /* Registers the subroutine changed that were unknown stay unknown; ret xN killed r.
     */
    if (retreg >= 0 && retreg != 30)
        gkill(g, retreg);
    if (ret == 1)
        gkill(g, 30); /* RETAA authenticates the target but leaves x30 signed */
    memcpy(g->sinit, s->sinit, sizeof(g->sinit));
    g->dyn += 4 * g->mult;
    free(s);
    free(s2);
}

/* Indirect jump to an out-of-line pad that jumps back. */
static void
gen_indirect(gen_t *g)
{
    asm_t *a = g->a;
    int t = pick_dst(g);
    if (t < 0)
        return;
    int pad = lab_new(a), back = lab_new(a);
    int kind =
        (int)rn(&g->r, feat.pauth ? 4 : 2); /* 0 br, 1 ret xN, 2 braa/ab, 3 braaz */
    int key = (int)rn(&g->r, 2), m = 31;
    int at = emit(a, 0x10000000u | U(t), "adr %s, L%d", XR(t), pad);
    fixup(a, at, pad, FX_ADR);
    gkill(g, t);
    int wild = 0;
    if (kind == 2) {
        m = rp(&g->r, 40) ? 31 : pick_src(g);
        if (m == t)
            m = 31;
        emit(a, 0xDAC10000u | U(key) << 10 | U(m) << 5 | U(t), "paci%c %s, %s",
             key ? 'b' : 'a', XR(t), XS(m));
        if (fault_budget(g) && rp(&g->r, 5) && g->depth == 0)
            wild = 1;
    } else if (kind == 3)
        emit(a, 0xDAC10000u | U(8 + key) << 10 | 31u << 5 | U(t), "paciz%c %s",
             key ? 'b' : 'a', XR(t));
    if (kind == 0)
        emit(a, 0xd61f0000u | U(t) << 5, "br %s", XR(t));
    else if (kind == 1)
        emit(a, 0xd65f0000u | U(t) << 5, "ret %s", XR(t));
    else if (kind == 2) {
        /* Wrong key on purpose: a wild branch that ends the block via the handler. */
        at = emit(a, 0xd71f0800u | U(wild ? !key : key) << 10 | U(t) << 5 | U(m),
                  "bra%c %s, %s%s", (wild ? !key : key) ? 'b' : 'a', XR(t), XS(m),
                  wild ? " (auth fails)" : "");
        if (wild) {
            g->wild_branch = 1;
            note_fault(g, at);
        }
    } else
        emit(a, 0xd61f081fu | U(key) << 10 | U(t) << 5, "bra%cz %s", key ? 'b' : 'a',
             XR(t));
    lab_bind(a, pad);
    if (rp(&g->r, 30))
        emit(a, 0xd503249fu, "bti j");
    forget_pac(g);
    snap_t *s = malloc(sizeof(*s));
    uint64_t saved_ceiling = g->ceiling;
    g->ceiling = SPV;
    snap(g, s);
    g->depth++;
    gen_seq(g, 1 + (int)rn(&g->r, 5));
    g->depth--;
    restore_to(g, s);
    memcpy(g->sinit, s->sinit, sizeof(g->sinit));
    g->ceiling = saved_ceiling;
    at = emit(a, 0x14000000u, "b L%d", back);
    fixup(a, at, back, FX_B26);
    lab_bind(a, back);
    g->dyn += 4 * g->mult;
    free(s);
}

static void
gen_one(gen_t *g)
{
    int r = (int)rn(&g->r, 100);
    if (r < 26)
        gen_alu(g);
    else if (r < 42)
        gen_fp(g);
    else if (r < 53)
        gen_ldst_single(g);
    else if (r < 60)
        gen_ldst_pair(g);
    else if (r < 64)
        gen_simd_struct(g);
    else if (r < 69)
        gen_atomic(g);
    else if (r < 72)
        gen_literal(g);
    else if (r < 74)
        gen_ldraa(g);
    else if (r < 76)
        gen_cacheop(g);
    else if (r < 82)
        gen_sys(g);
    else if (r < 89)
        gen_pac(g);
    else if (r < 91 && g->allow_excl)
        gen_excl_loop(g);
    else
        gen_alu(g);
}

static void
gen_seq(gen_t *g, int len)
{
    for (int i = 0; i < len && !g->a->overflow && g->a->n < MAXW - 400; i++) {
        int r = (int)rn(&g->r, 100);
        if (g->depth < 3 && r < 5 && budget_ok(g, 2))
            gen_loop(g);
        else if (g->depth < 3 && r < 9)
            gen_if(g);
        else if (g->depth < 3 && r < 12)
            gen_call(g);
        else if (g->depth < 3 && r < 14)
            gen_indirect(g);
        else
            gen_one(g);
    }
}

/***************************************************************************
 * Block construction.
 */

/* Register state layout shared with the harness. */
typedef struct {
    uint64_t x[31];
    uint64_t sp;
    uint64_t nzcv;
    uint64_t fpsr;
    uint64_t fpcr;
    uint64_t pad;
    __uint128_t v[32];
} cpu_state_t;

#define CS_SP 248
#define CS_NZCV 256
#define CS_FPSR 264
#define CS_FPCR 272
#define CS_V 288

typedef struct {
    cpu_state_t in;
    cpu_state_t out;
    uint64_t host_sp;
    uint64_t host_ret;
    /* The run id while the host trampoline runs the block, else -1 (written by
     * run_code() only, so that signal contexts can be attributed exactly).
     */
    volatile int64_t run_id;
} harn_t;

#define HARN_IN (HARN_ADDR + 0)
#define HARN_OUT (HARN_ADDR + sizeof(cpu_state_t))
#define HARN_HOST (HARN_ADDR + 2 * sizeof(cpu_state_t))

typedef struct {
    uint64_t seed;
    asm_t a;
    int block_start, exit_start; /* instruction indices */
    cpu_state_t in;
    uint8_t d[D_SIZE];
    uint8_t stk[STK_CMP];
    uint8_t sinit_final[STK_CMP];
    int nfaults_planned, has_excl, wild_branch;
    int ncas, ncas_primed;
    int xres, vres;
    uint64_t dyn_est;
} block_t;

typedef struct {
    int allow_excl, allow_faults, allow_brk, allow_svc, allow_fpcr, allow_ctr;
    int traced, reserve, outer_loop;
    int len;
    uint64_t dyn_max;
} gen_opts_t;

static void
emit_ld_harn(asm_t *a, int rd, uint64_t addr)
{
    for (int hw = 0; hw < 4; hw++) {
        uint32_t c = (addr >> (16 * hw)) & 0xffff;
        if (hw == 0)
            emit(a, e_movw(1, 2, 0, c, rd), "movz %s, #0x%x", XR(rd), c);
        else if (c)
            emit(a, e_movw(1, 3, hw, c, rd), "movk %s, #0x%x, lsl #%d", XR(rd), c,
                 16 * hw);
    }
}

/* The entry and exit stubs are modelled like the block (meminfo_t), so that a
 * whole run, stubs included, has an access model; a64_dr_prop itself only
 * checks the block and the first exit instruction.
 */
static int
emit_m(asm_t *a, uint32_t w, int base, int64_t off, int wb, int acc, uint64_t size,
       const char *txt)
{
    int at = emit(a, w, "%s", txt);
    meminfo_t *m = &a->mi[at];
    m->valid = 1;
    m->base = (uint8_t)base;
    m->idx = -1;
    m->off = (int32_t)off;
    m->wb = (uint8_t)wb;
    set_acc(a, at, acc, size);
    return at;
}

static void
emit_entry(asm_t *a)
{
    char t[TXTLEN];
    emit_ld_harn(a, 30, HARN_IN);
    for (int i = 0; i < 32; i += 2) {
        snprintf(t, sizeof(t), "ldp q%d, q%d, [x30, #%d]", i, i + 1, CS_V + 16 * i);
        emit_m(a, e_ldst_pair(2, 1, 2, 1, (CS_V + 16 * i) / 16, i + 1, 30, i), 30,
               CS_V + 16 * i, 0, MA_R, 32, t);
    }
    emit_m(a, e_ldst_uimm(3, 0, 1, CS_FPSR / 8, 30, 0), 30, CS_FPSR, 0, MA_R, 8,
           "ldr x0, [x30, #fpsr]");
    emit(a, e_msr(SR_FPSR, 0), "msr fpsr, x0");
    emit_m(a, e_ldst_uimm(3, 0, 1, CS_FPCR / 8, 30, 0), 30, CS_FPCR, 0, MA_R, 8,
           "ldr x0, [x30, #fpcr]");
    emit(a, e_msr(SR_FPCR, 0), "msr fpcr, x0");
    emit_m(a, e_ldst_uimm(3, 0, 1, CS_NZCV / 8, 30, 0), 30, CS_NZCV, 0, MA_R, 8,
           "ldr x0, [x30, #nzcv]");
    emit(a, e_msr(SR_NZCV, 0), "msr nzcv, x0");
    emit_m(a, e_ldst_uimm(3, 0, 1, CS_SP / 8, 30, 0), 30, CS_SP, 0, MA_R, 8,
           "ldr x0, [x30, #sp]");
    emit(a, e_addsub_imm(1, 0, 0, 0, 0, 0, 31), "mov sp, x0");
    for (int i = 0; i < 30; i += 2) {
        snprintf(t, sizeof(t), "ldp x%d, x%d, [x30, #%d]", i, i + 1, i * 8);
        emit_m(a, e_ldst_pair(2, 0, 2, 1, i, i + 1, 30, i), 30, i * 8, 0, MA_R, 16, t);
    }
    emit_m(a, e_ldst_uimm(3, 0, 1, 30, 30, 30), 30, 240, 0, MA_R, 8,
           "ldr x30, [x30, #240]");
}

static void
emit_exit(asm_t *a)
{
    char t[TXTLEN];
    int at = emit_m(a, e_ldst_pair(2, 0, 3, 0, -2, 1, 31, 0), 31, -16, 1, MA_W, 16,
                    "stp x0, x1, [sp, #-16]!");
    /* The first exit instruction is part of the checked range: model it. */
    a->mi[at].must = 1;
    emit_ld_harn(a, 0, HARN_OUT);
    for (int i = 2; i < 30; i += 2) {
        snprintf(t, sizeof(t), "stp x%d, x%d, [x0, #%d]", i, i + 1, i * 8);
        emit_m(a, e_ldst_pair(2, 0, 2, 0, i, i + 1, 0, i), 0, i * 8, 0, MA_W, 16, t);
    }
    emit_m(a, e_ldst_uimm(3, 0, 0, 30, 0, 30), 0, 240, 0, MA_W, 8, "str x30, [x0, #240]");
    emit_m(a, e_ldst_pair(2, 0, 1, 1, 2, 3, 31, 2), 31, 0, 2, MA_R, 16,
           "ldp x2, x3, [sp], #16");
    emit_m(a, e_ldst_pair(2, 0, 2, 0, 0, 3, 0, 2), 0, 0, 0, MA_W, 16, "stp x2, x3, [x0]");
    emit(a, e_addsub_imm(1, 0, 0, 0, 0, 31, 1), "mov x1, sp");
    emit_m(a, e_ldst_uimm(3, 0, 0, CS_SP / 8, 0, 1), 0, CS_SP, 0, MA_W, 8,
           "str x1, [x0, #sp]");
    emit(a, e_mrs(SR_NZCV, 1), "mrs x1, nzcv");
    emit_m(a, e_ldst_uimm(3, 0, 0, CS_NZCV / 8, 0, 1), 0, CS_NZCV, 0, MA_W, 8,
           "str x1, [x0, #nzcv]");
    emit(a, e_mrs(SR_FPSR, 1), "mrs x1, fpsr");
    emit_m(a, e_ldst_uimm(3, 0, 0, CS_FPSR / 8, 0, 1), 0, CS_FPSR, 0, MA_W, 8,
           "str x1, [x0, #fpsr]");
    emit(a, e_mrs(SR_FPCR, 1), "mrs x1, fpcr");
    emit_m(a, e_ldst_uimm(3, 0, 0, CS_FPCR / 8, 0, 1), 0, CS_FPCR, 0, MA_W, 8,
           "str x1, [x0, #fpcr]");
    for (int i = 0; i < 32; i += 2) {
        snprintf(t, sizeof(t), "stp q%d, q%d, [x0, #%d]", i, i + 1, CS_V + 16 * i);
        emit_m(a, e_ldst_pair(2, 1, 2, 0, (CS_V + 16 * i) / 16, i + 1, 0, i), 0,
               CS_V + 16 * i, 0, MA_W, 32, t);
    }
    emit_ld_harn(a, 1, HARN_HOST);
    emit_m(a, e_ldst_uimm(3, 0, 1, 0, 1, 2), 1, 0, 0, MA_R, 8, "ldr x2, [x1]");
    emit(a, e_addsub_imm(1, 0, 0, 0, 0, 2, 31), "mov sp, x2");
    emit_m(a, e_ldst_uimm(3, 0, 1, 1, 1, 30), 1, 8, 0, MA_R, 8, "ldr x30, [x1, #8]");
    emit(a, 0xd65f03c0u, "ret");
}

static __uint128_t
rand_vreg(rng_t *r)
{
    uint64_t lo = rnext(r), hi = rnext(r);
    switch (rn(r, 6)) {
    case 0: lo = 0x3ff0000000000000ULL + (rnext(r) & 0xfffffffffffULL); break; /* ~1.x */
    case 1: lo = 0x7ff8000000000000ULL | (rnext(r) & 0xff); break;             /* NaN */
    case 2: lo = rnext(r) & 0x000fffffffffffffULL; break; /* denormal */
    case 3:
        lo = 0x3f8000003f800000ULL ^ (rnext(r) & 0x00ff00ff00ff00ffULL);
        break; /* floats */
    default: break;
    }
    return (__uint128_t)hi << 64 | lo;
}

/* Generate a block from seed.  Returns 0 on failure (e.g. overflow). */
static int
gen_block(block_t *b, uint64_t seed, const gen_opts_t *o)
{
    gen_t *g = calloc(1, sizeof(*g));
    asm_t *a = &b->a;
    memset(a, 0, sizeof(*a));
    b->seed = seed;
    g->r.s = seed * 0x2545f4914f6cdd1dULL + 12345;
    g->a = a;
    g->allow_excl = o->allow_excl && !o->traced;
    g->allow_faults = o->allow_faults;
    g->allow_brk = o->allow_brk && !o->traced;
    g->allow_svc = o->allow_svc;
    g->allow_fpcr = o->allow_fpcr;
    g->allow_ctr = o->allow_ctr;
    g->traced = o->traced;
    g->dyn_max = o->dyn_max;
    g->xres = g->vres = -1;
    if (o->reserve) {
        static const int pref[] = { 28, 30, 2, 0, 18, 29 };
        g->xres = rp(&g->r, 60) ? pref[rn(&g->r, 6)] : (int)rn(&g->r, 31);
        g->vres = (int)rn(&g->r, 32);
    }
    g->mult = 1;
    g->dczva_ok = gen_dczva_ok;
    /* Initial state. */
    cpu_state_t *in = &b->in;
    memset(in, 0, sizeof(*in));
    for (int i = 0; i < 31; i++)
        in->x[i] = rval(&g->r);
    for (int i = 0; i < 32; i++)
        in->v[i] = rand_vreg(&g->r);
    in->nzcv = (uint64_t)rn(&g->r, 16) << 28;
    in->fpsr = rand_fpsr(g);
    in->fpcr = o->allow_fpcr ? rand_fpcr(g) : 0;
    in->sp = STK_TOP - 512 - 16 * rn(&g->r, 32);
    for (int i = 0; i < (int)D_SIZE; i++)
        b->d[i] = (uint8_t)rnext(&g->r);
    for (int i = 0; i < (int)STK_CMP; i++)
        b->stk[i] = (uint8_t)rnext(&g->r);
    /* A few registers start as pointers. */
    int np = 2 + (int)rn(&g->r, 4);
    for (int i = 0; i < np; i++) {
        int r = rp(&g->r, 35) ? 28 : (int)rn(&g->r, 31);
        if (r == g->xres)
            continue;
        in->x[r] = D_ADDR + (rn(&g->r, (uint32_t)D_SIZE) & ~7ULL);
    }
    for (int r = 0; r < 31; r++) {
        g->known[r] = 1;
        g->val[r] = in->x[r];
    }
    if (g->xres >= 0) {
        g->known[g->xres] = 0;
        g->lock[g->xres] = 1;
    }
    g->known[31] = 1;
    g->val[31] = in->sp;
    for (uint64_t x = in->sp; x < STK_TOP; x++)
        g->sinit[x - STK_CMP_LO] = 1;
    g->ceiling = STK_TOP - 64;

    emit_entry(a);
    b->block_start = a->n;
    /* Literal pool. */
    int over = lab_new(a);
    int at = emit(a, 0x14000000u, "b L%d", over);
    fixup(a, at, over, FX_B26);
    if (a->n % 2)
        emit(a, 0xd503201fu, "nop (align)");
    g->lit_lab = lab_new(a);
    lab_bind(a, g->lit_lab);
    for (int i = 0; i < 8; i++) {
        uint64_t v =
            rp(&g->r, 50) ? D_ADDR + (rn(&g->r, (uint32_t)D_SIZE) & ~7ULL) : rval(&g->r);
        g->litval[i] = v;
        int k = emit(a, U(v), ".word 0x%08x (lit%d lo)", U(v), i);
        a->isdata[k] = 1;
        k = emit(a, U(v >> 32), ".word 0x%08x (lit%d hi)", U(v >> 32), i);
        a->isdata[k] = 1;
    }
    lab_bind(a, over);
    if (o->outer_loop) {
        /* Long-running blocks: wrap the body in a counted loop whose count is
         * patched in once the body's dynamic size is known.
         */
        int c = -1;
        while (c < 0 || c == g->xres)
            c = (int)rn(&g->r, 29);
        int movat = emit(a, e_movw(1, 2, 0, 1, c), "movz %s, #N (outer count)", XR(c));
        g->lock[c]++;
        forget_pac(g);
        gkill(g, c);
        snap_t *s = malloc(sizeof(*s));
        uint64_t saved_ceiling = g->ceiling;
        g->ceiling = SPV;
        snap(g, s);
        int head = lab_new(a);
        lab_bind(a, head);
        uint64_t dyn0 = g->dyn;
        g->depth++;
        gen_seq(g, o->len);
        g->depth--;
        restore_to(g, s);
        uint64_t body = g->dyn - dyn0 + 2;
        uint64_t n = o->dyn_max / (body == 0 ? 1 : body);
        if (n < 1)
            n = 1;
        if (n > 0xffff)
            n = 0xffff;
        a->w[movat] = e_movw(1, 2, 0, (uint32_t)n, c);
        snprintf(a->txt[movat], TXTLEN, "movz %s, #%llu (outer count)", XR(c),
                 (unsigned long long)n);
        emit(a, e_addsub_imm(1, 1, 1, 0, 1, c, c), "subs %s, %s, #1", XR(c), XR(c));
        int at2 = emit(a, 0x54000000u | 1u, "b.ne L%d", head);
        fixup(a, at2, head, FX_B19);
        g->lock[c]--;
        g->ceiling = saved_ceiling;
        setk(g, c, 0);
        g->dyn = dyn0 + body * n;
        free(s);
    } else
        gen_seq(g, o->len);
    g->exit_lab = lab_new(a);
    lab_bind(a, g->exit_lab);
    b->exit_start = a->n;
    emit_exit(a);
    int ok = !a->overflow && resolve(a);
    b->nfaults_planned = g->nfaults_planned;
    b->has_excl = g->has_excl;
    b->wild_branch = g->wild_branch;
    b->ncas = g->ncas;
    b->ncas_primed = g->ncas_primed;
    b->xres = g->xres;
    b->vres = g->vres;
    b->dyn_est = g->dyn;
    memcpy(b->sinit_final, g->sinit, sizeof(g->sinit));
    /* The exit stub stores 16 bytes below the final SP. */
    free(g);
    return ok;
}

static void
print_block(FILE *f, block_t *b)
{
    asm_t *a = &b->a;
    fprintf(f,
            "  block seed=%llu: %d instrs, entry=0x%llx block=0x%llx exit=0x%llx xres=%d "
            "vres=%d\n",
            (unsigned long long)b->seed, a->n, (unsigned long long)CODE_BASE,
            (unsigned long long)pc_of(b->block_start),
            (unsigned long long)pc_of(b->exit_start), b->xres, b->vres);
    for (int i = b->block_start; i < b->exit_start; i++) {
        for (int l = 0; l < a->nlab; l++) {
            if (a->lab[l] == i)
                fprintf(f, "  L%d:\n", l);
        }
        fprintf(f, "    %llx: %08x  %s%s\n", (unsigned long long)pc_of(i), a->w[i],
                a->txt[i], a->fault_ok[i] ? "   <planned fault>" : "");
    }
}


#endif /* A64_DR_PROP_GEN_H */
