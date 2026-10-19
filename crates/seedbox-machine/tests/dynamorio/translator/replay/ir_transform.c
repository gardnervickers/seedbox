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

/* Metamorphic property tests of DR's AArch64 IR transformations.  No CPU
 * oracle: each property relates two IR states that a transformation must
 * keep equivalent (or change in exactly one documented way), so a bug in
 * cloning, register replacement, raw-bits bookkeeping, relocation, label
 * fixups or the regdeps projection shows up as an inconsistency.  Inputs are
 * the words of suite/tests/api/dis-a64*.txt (not SVE), random operand-field
 * mutants of them, instructions built with the INSTR_CREATE_ macros, and
 * random instruction lists with labels.  See README.md.
 *
 * Equality is canonical, not byte equality: a re-encoding must be the word
 * the vectors list (the word or its listed canonical form, as dis-a64.c
 * checks), and for other inputs a fixed point of decode+encode.  IR equality
 * is field by field ("strict"): it also compares what instr_same() ignores
 * on AArch64 (extend type, shift, pre-index state, element sizes, flags).
 *
 * Usage: a64_ir_transform [options] <dis-a64*.txt...>
 *   -seed S       base seed (default 1)
 *   -n N          mutants (default 400000)
 *   -created N    INSTR_CREATE_ instructions (default 40000)
 *   -ilists N     instruction lists (default 8000)
 *   -props a,b    only these properties
 *   -word W       run every property on one word (with -seed) and exit
 *   -max-print M  failures printed per property (default 10)
 *   -strict       known deviations are failures
 *   -no-floors    do not enforce the minimum check counts
 *   -v            print known deviations too
 * Environment: A64_IRT_SEED / A64_IRT_STRICT=1.
 *
 * Every failure prints one line:
 *   FAIL <property> word=0x... seed=0x... "<disassembly>": details
 * and "a64_ir_transform -word 0x... -seed 0x..." reruns that word.
 */

#ifndef _GNU_SOURCE
#    define _GNU_SOURCE
#endif
#include "dr_api.h"

#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#ifdef DYNAMORIO_STANDALONE
static void *replay_context;
# define GD replay_context
#else
# define GD GLOBAL_DCONTEXT
#endif

/* Where decode_from_copy() says the words are: nothing is mapped here. */
#define P_BASE 0x400000000000ULL

/***************************************************************************
 * Random numbers (splitmix64): every case is a function of its seed.
 */
typedef struct {
    uint64_t s;
} rng_t;

static uint64_t
rnd(rng_t *r)
{
    uint64_t z = (r->s += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static uint64_t
rnd_n(rng_t *r, uint64_t n)
{
    return rnd(r) % n;
}

static rng_t
rng_for(uint64_t seed, uint64_t stream)
{
    rng_t r = { seed * 0x2545f4914f6cdd1dULL + stream * 0x9e3779b97f4a7c15ULL };
    rnd(&r);
    return r;
}

/***************************************************************************
 * Properties, counters and reporting.
 */
typedef enum {
    P_CANON,   /* decode+encode gives the canonical word */
    P_CLONE,   /* a clone outlives its original unchanged */
    P_RAWBITS, /* translation / raw-bits changes are attribution only */
    P_RELOC,   /* encode at another pc: same absolute targets, or rejected */
    P_RENAME,  /* register permutation: exact, query-consistent, invertible */
    P_REGDEPS, /* regdeps projection against an independent model */
    P_ILIST,   /* instrlist encode/decode with labels */
    P_CREATE,  /* INSTR_CREATE_ instructions encode and decode back */
    NUM_PROPS
} prop_t;

static const char *const prop_names[NUM_PROPS] = {
    "canon", "clone", "rawbits", "reloc", "rename", "regdeps", "ilist", "create",
};

/* Minimum checks per property for the default run: a run that checked less
 * did not test what it claims to.
 */
static const uint64_t prop_floor[NUM_PROPS] = {
    250000, 250000, 250000, 1500000, 250000, 250000, 6000, 30000,
};

static uint64_t checked[NUM_PROPS], failed[NUM_PROPS], known_cnt[NUM_PROPS];

/* Known deviations: named, counted, dated.  -strict makes them failures. */
typedef enum {
    K_RAW_PCREL,      /* i#4016 */
    K_REGDEPS_OPNDS,  /* regdeps encoding holds at most 8 registers */
    K_CREATE_SHIFTED, /* INSTR_CREATE_ marks shifted/extended registers */
    NUM_KNOWN
} known_t;

static const struct {
    const char *name, *since, *reason;
} known_info[NUM_KNOWN] = {
    { "i4016-raw-pcrel", "2026-10-14",
      "encoding a PC-relative instruction whose raw bits are still valid at another "
      "final_pc copies the bytes without re-relativizing them (TODO i#4016 in "
      "copy_and_re_relativize_raw_instr); with the raw bits invalidated the target is "
      "kept" },
    { "regdeps-max-operands", "2026-10-14",
      "the DR_ISA_REGDEPS encoding holds at most REGDEPS_MAX_NUM_OPNDS (8) registers, "
      "so instr_convert_to_isa_regdeps() of an AArch64 instruction with more distinct "
      "registers (LD64B/ST64B*, which name eight data registers plus the base) cannot "
      "be encoded" },
    { "create-shift-flags", "2026-10-14",
      "INSTR_CREATE_ macros mark a shifted or extended register operand "
      "DR_OPND_SHIFTED/DR_OPND_EXTENDED; the decoder never sets those flags, so created "
      "and decoded instructions differ in them (instr_same() ignores flags; the "
      "encodings are identical)" },
};
static uint64_t known_hits[NUM_KNOWN];

/* Other counts the summary shows (not properties). */
static uint64_t n_vectors, n_vector_skipped, n_mutants, n_mutant_rejected, n_created,
    n_ilists, n_reloc_pcrel, n_reloc_rejected, n_rename_inplace, n_rename_api[3],
    n_rename_skipped, n_ilist_rejected, n_ilist_far, n_regdeps_learned,
    n_regdeps_unmapped, n_clone_inplace, n_same_checked, n_same_prepost;

static bool prop_on[NUM_PROPS];
static int opt_max_print = 10;
static bool opt_strict, opt_verbose;

static uint32_t g_word; /* the case's word, for reports */
static uint64_t g_seed; /* the case's seed */
static char g_dis[128]; /* its disassembly */
static char g_ctx[256]; /* printed if the case crashes */

static void
set_case(uint32_t word, uint64_t seed, const char *what)
{
    g_word = word;
    g_seed = seed;
    byte w[4];
    memcpy(w, &word, 4);
    g_dis[0] = '\0';
    disassemble_to_buffer(GD, w, (byte *)P_BASE, false, false, g_dis, sizeof(g_dis),
                          NULL);
    size_t l = strlen(g_dis);
    while (l > 0 && (g_dis[l - 1] == '\n' || g_dis[l - 1] == ' '))
        g_dis[--l] = '\0';
    snprintf(g_ctx, sizeof(g_ctx), "%s word=0x%08x seed=0x%llx \"%s\"", what, word,
             (unsigned long long)seed, g_dis);
}

static void
crash_handler(int sig)
{
    static const char msg[] = "\nCRASH while running case: ";
    if (write(2, msg, sizeof(msg) - 1) < 0 || write(2, g_ctx, strlen(g_ctx)) < 0 ||
        write(2, "\n", 1) < 0) {
        /* Nothing else to do. */
    }
    signal(sig, SIG_DFL);
    raise(sig);
}

static void
report(prop_t p, const char *fmt, ...)
{
    failed[p]++;
    if (failed[p] > (uint64_t)opt_max_print)
        return;
    char msg[768];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(msg, sizeof(msg), fmt, ap);
    va_end(ap);
    printf("FAIL %s word=0x%08x seed=0x%llx \"%s\": %s\n", prop_names[p], g_word,
           (unsigned long long)g_seed, g_dis, msg);
    fflush(stdout);
}

static void
known(prop_t p, known_t k, const char *fmt, ...)
{
    if (opt_strict) {
        char msg[512];
        va_list ap;
        va_start(ap, fmt);
        vsnprintf(msg, sizeof(msg), fmt, ap);
        va_end(ap);
        report(p, "[%s] %s", known_info[k].name, msg);
        return;
    }
    known_cnt[p]++;
    known_hits[k]++;
    if (!opt_verbose)
        return;
    char msg[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(msg, sizeof(msg), fmt, ap);
    va_end(ap);
    printf("KNOWN %s [%s] word=0x%08x seed=0x%llx \"%s\": %s\n", prop_names[p],
           known_info[k].name, g_word, (unsigned long long)g_seed, g_dis, msg);
}

static void
dis_instr(instr_t *in, char *buf, size_t n)
{
    buf[0] = '\0';
    instr_disassemble_to_buffer(GD, in, buf, n);
}

/***************************************************************************
 * Decoding and encoding.
 */

/* A page whose words are decoded in place, so their raw bits stay valid. */
static uint32_t *g_slots;
#define NUM_SLOTS 1024
#define SLOT_ILIST 512 /* slots from here on are for instruction lists */

static instr_t *
dec_copy(uint32_t word, uint64_t pc)
{
    instr_t *in = instr_create(GD);
    if (decode_from_copy(GD, (byte *)&word, (byte *)pc, in) == NULL) {
        instr_destroy(GD, in);
        return NULL;
    }
    return in;
}

static instr_t *
dec_inplace(uint32_t word, int slot)
{
    g_slots[slot] = word;
    instr_t *in = instr_create(GD);
    if (decode(GD, (byte *)&g_slots[slot], in) == NULL) {
        instr_destroy(GD, in);
        return NULL;
    }
    return in;
}

/* Encodes one instruction as if at pc; false if the encoder rejects it. */
static bool
enc_at(instr_t *in, uint64_t pc, uint32_t *word)
{
    uint32_t buf[2] = { 0, 0 };
    byte *end = instr_encode_to_copy(GD, in, (byte *)buf, (byte *)pc);
    if (end == NULL)
        return false;
    if (end != (byte *)buf + 4) {
        *word = 0;
        return false;
    }
    *word = buf[0];
    return true;
}

static bool
reg_is_sve(reg_id_t r)
{
    return (r >= DR_REG_Z0 && r <= DR_REG_Z31) || (r >= DR_REG_P0 && r <= DR_REG_P15) ||
        r == DR_REG_FFR;
}

/* SVE (and SME) is out of scope: those instructions use Z or P registers. */
static bool
uses_sve(instr_t *in)
{
    for (int k = 0; k < 2; k++) {
        int n = k == 0 ? instr_num_srcs(in) : instr_num_dsts(in);
        for (int i = 0; i < n; i++) {
            opnd_t o = k == 0 ? instr_get_src(in, i) : instr_get_dst(in, i);
            for (int j = 0; j < opnd_num_regs_used(o); j++) {
                if (reg_is_sve(opnd_get_reg_used(o, j)))
                    return true;
            }
        }
    }
    return false;
}

static bool
sve_or_sme_space(uint32_t w)
{
    uint32_t op0 = (w >> 25) & 0xf;
    return op0 == 0x2 || ((w & 0x80000000) != 0 && op0 == 0x0);
}

/***************************************************************************
 * Strict IR equality.
 */
static const char *
szname(opnd_size_t sz)
{
    static char buf[4][16];
    static int i;
    i = (i + 1) % 4;
    if (sz == OPSZ_NA)
        return "NA";
    snprintf(buf[i], sizeof(buf[i]), "%ub", opnd_size_in_bytes(sz));
    return buf[i];
}
#define WHY(...)                                \
    do {                                        \
        if (why != NULL)                        \
            snprintf(why, whylen, __VA_ARGS__); \
        return false;                           \
    } while (0)

/* Probe zero-offset pre/post-index through an LDR writeback encoding. This
 * observes the distinction the original private pre_index comparison intended.
 * Indexed operands have no pre/post form; their inactive representation bits
 * have no public oracle (see provenance.txt).
 */
static bool
public_pre_index(opnd_t mem)
{
    if (opnd_get_index(mem) != DR_REG_NULL)
        return false;
    opnd_set_size(&mem, OPSZ_8);
    opnd_set_flags(&mem, 0);
    reg_id_t base = opnd_get_base(mem);
    if (base != DR_REG_X1)
        opnd_replace_reg(&mem, base, DR_REG_X1);
    instr_t *probe = INSTR_CREATE_ldr_imm(GD, opnd_create_reg(DR_REG_X0),
        opnd_create_reg(DR_REG_X1), mem, OPND_CREATE_INT(0));
    uint32_t bits = 0;
    byte *end = instr_encode_to_copy(GD, probe, (byte *)&bits, (byte *)P_BASE);
    instr_destroy(GD, probe);
    if (end != (byte *)&bits + 4 ||
        (bits != 0xf8400420 && bits != 0xf8400c20)) {
        fprintf(stderr, "pre/post public probe failed: %08x\n", bits);
        exit(1);
    }
    return bits == 0xf8400c20;
}

static bool
opnd_strict_mapped(opnd_t a, opnd_t b, bool relax_imm_size, char *why, size_t whylen,
                   reg_id_t want_reg, reg_id_t want_base, reg_id_t want_index)
{
    if (opnd_is_null(a) != opnd_is_null(b) ||
        opnd_is_immed_int(a) != opnd_is_immed_int(b) ||
        opnd_is_immed_float(a) != opnd_is_immed_float(b) ||
        opnd_is_immed(a) != opnd_is_immed(b) ||
        opnd_is_pc(a) != opnd_is_pc(b) || opnd_is_instr(a) != opnd_is_instr(b) ||
        opnd_is_reg(a) != opnd_is_reg(b) ||
        opnd_is_base_disp(a) != opnd_is_base_disp(b) ||
        opnd_is_rel_addr(a) != opnd_is_rel_addr(b) ||
        opnd_is_mem_instr(a) != opnd_is_mem_instr(b))
        WHY("operand category");
    if (opnd_is_null(a))
        return true;
    if (opnd_is_immed_int(a)) {
        if (opnd_get_immed_int(a) != opnd_get_immed_int(b))
            WHY("immediate %lld vs %lld", (long long)opnd_get_immed_int(a),
                (long long)opnd_get_immed_int(b));
        if (!relax_imm_size && opnd_get_size(a) != opnd_get_size(b))
            WHY("immediate size %s vs %s", szname(opnd_get_size(a)),
                szname(opnd_get_size(b)));
        if (!relax_imm_size && opnd_get_flags(a) != opnd_get_flags(b))
            WHY("immediate flags 0x%x vs 0x%x", opnd_get_flags(a), opnd_get_flags(b));
        return true;
    }
    if (opnd_is_immed_float(a)) {
        float x = opnd_get_immed_float(a), y = opnd_get_immed_float(b);
        if (memcmp(&x, &y, sizeof(x)) != 0)
            WHY("float immediate");
        return true;
    }
    if (opnd_is_immed(a) && !opnd_is_immed_float(a)) {
        double x = opnd_get_immed_double(a), y = opnd_get_immed_double(b);
        if (memcmp(&x, &y, sizeof(x)) != 0)
            WHY("double immediate");
        return true;
    }
    if (opnd_is_pc(a)) {
        if (opnd_get_pc(a) != opnd_get_pc(b))
            WHY("pc %p vs %p", opnd_get_pc(a), opnd_get_pc(b));
        return true;
    }
    if (opnd_is_instr(a)) {
        if (opnd_get_instr(a) != opnd_get_instr(b) ||
            opnd_get_shift(a) != opnd_get_shift(b))
            WHY("instr target");
        return true;
    }
    if (opnd_is_reg(a)) {
        if (want_reg != opnd_get_reg(b))
            WHY("register %s vs %s", get_register_name(opnd_get_reg(a)),
                get_register_name(opnd_get_reg(b)));
        if (opnd_get_size(a) != opnd_get_size(b))
            WHY("register size %s vs %s", szname(opnd_get_size(a)),
                szname(opnd_get_size(b)));
        if (opnd_get_vector_element_size(a) !=
            opnd_get_vector_element_size(b)) {
            WHY("%s element size %s vs %s", get_register_name(opnd_get_reg(a)),
                szname(opnd_get_vector_element_size(a)),
                szname(opnd_get_vector_element_size(b)));
        }
        if (opnd_get_flags(a) != opnd_get_flags(b))
            WHY("%s flags 0x%x vs 0x%x", get_register_name(opnd_get_reg(a)),
                opnd_get_flags(a), opnd_get_flags(b));
        return true;
    }
    if (opnd_is_base_disp(a)) {
        bool sa, sb;
        uint aa, ab;
        dr_extend_type_t ea = opnd_get_index_extend(a, &sa, &aa);
        dr_extend_type_t eb = opnd_get_index_extend(b, &sb, &ab);
        if (want_base != opnd_get_base(b))
            WHY("base %s vs %s", get_register_name(opnd_get_base(a)),
                get_register_name(opnd_get_base(b)));
        if (want_index != opnd_get_index(b))
            WHY("index %s vs %s", get_register_name(opnd_get_index(a)),
                get_register_name(opnd_get_index(b)));
        if (opnd_get_disp(a) != opnd_get_disp(b))
            WHY("disp %d vs %d", opnd_get_disp(a), opnd_get_disp(b));
        if (opnd_get_size(a) != opnd_get_size(b))
            WHY("memory size %s vs %s", szname(opnd_get_size(a)),
                szname(opnd_get_size(b)));
        if (opnd_get_index(a) != DR_REG_NULL && (ea != eb || sa != sb || aa != ab))
            WHY("index extend %d/%d/%u vs %d/%d/%u", ea, sa, aa, eb, sb, ab);
        /* Pre-index state only matters (and is only kept) with a zero offset. */
        if (opnd_get_disp(a) == 0 &&
            public_pre_index(a) != public_pre_index(b))
            WHY("pre-index %d vs %d", public_pre_index(a),
                public_pre_index(b));
        /* The element size is only defined for vector (SVE) base/index registers:
         * opnd_create_base_disp() leaves it uninitialized otherwise.
         */
        if ((reg_is_sve(opnd_get_base(a)) || reg_is_sve(opnd_get_index(a))) &&
            opnd_get_vector_element_size(a) != opnd_get_vector_element_size(b))
            WHY("memory element size");
        if (opnd_get_flags(a) != opnd_get_flags(b))
            WHY("memory flags 0x%x vs 0x%x", opnd_get_flags(a), opnd_get_flags(b));
        return true;
    }
    if (opnd_is_rel_addr(a)) {
        if (opnd_get_addr(a) != opnd_get_addr(b))
            WHY("address %p vs %p", opnd_get_addr(a), opnd_get_addr(b));
        if (opnd_get_size(a) != opnd_get_size(b))
            WHY("rel-addr size");
        return true;
    }
    if (opnd_is_mem_instr(a)) {
        if (opnd_get_instr(a) != opnd_get_instr(b) ||
            opnd_get_mem_instr_disp(a) != opnd_get_mem_instr_disp(b))
            WHY("mem-instr");
        return true;
    }
    WHY("unhandled public operand category");
}

static bool
opnd_strict(opnd_t a, opnd_t b, bool relax, char *why, size_t len)
{
    return opnd_strict_mapped(a, b, relax, why, len,
        opnd_is_reg(a) ? opnd_get_reg(a) : DR_REG_NULL,
        opnd_is_base_disp(a) ? opnd_get_base(a) : DR_REG_NULL,
        opnd_is_base_disp(a) ? opnd_get_index(a) : DR_REG_NULL);
}

/* skip_src0: ignore source 0 (a branch target compared separately). */
static bool
instr_strict(instr_t *a, instr_t *b, bool relax_imm_size, bool skip_src0, char *why,
             size_t whylen)
{
    char w[256];
    if (instr_get_opcode(a) != instr_get_opcode(b))
        WHY("opcode %s vs %s", decode_opcode_name(instr_get_opcode(a)),
            decode_opcode_name(instr_get_opcode(b)));
    if (instr_num_srcs(a) != instr_num_srcs(b) || instr_num_dsts(a) != instr_num_dsts(b))
        WHY("operand counts %d/%d vs %d/%d", instr_num_srcs(a), instr_num_dsts(a),
            instr_num_srcs(b), instr_num_dsts(b));
    if (instr_get_predicate(a) != instr_get_predicate(b))
        WHY("predicate %d vs %d", instr_get_predicate(a), instr_get_predicate(b));
    if (instr_get_isa_mode(a) != instr_get_isa_mode(b))
        WHY("isa mode");
    for (int i = skip_src0 ? 1 : 0; i < instr_num_srcs(a); i++) {
        if (!opnd_strict(instr_get_src(a, i), instr_get_src(b, i), relax_imm_size, w,
                         sizeof(w)))
            WHY("src %d: %s", i, w);
    }
    for (int i = 0; i < instr_num_dsts(a); i++) {
        if (!opnd_strict(instr_get_dst(a, i), instr_get_dst(b, i), relax_imm_size, w,
                         sizeof(w)))
            WHY("dst %d: %s", i, w);
    }
    return true;
}

/***************************************************************************
 * Register permutations.
 *
 * A permutation maps register numbers within a class (GPR, SIMD&FP) and keeps
 * each register's width: X5->X9 implies W5->W9.  SP, ZR and system registers
 * are fixed.  GPR permutations keep pairs (2i, 2i+1) together (CASP and
 * friends need an even first register), with X30 fixed (the implicit link
 * register); SIMD&FP permutations are rotations, within each half of the
 * register file (indexed-element forms restrict Vm to V0-V15) or over all 32
 * when the instruction has a register list (consecutive modulo 32).
 */
typedef struct {
    int gpr[32], vec[32];
} perm_t;

static void
perm_identity(perm_t *p)
{
    for (int i = 0; i < 32; i++)
        p->gpr[i] = p->vec[i] = i;
}

static void
perm_inverse(const perm_t *p, perm_t *inv)
{
    for (int i = 0; i < 32; i++) {
        inv->gpr[p->gpr[i]] = i;
        inv->vec[p->vec[i]] = i;
    }
}

/* Register class and number: 1 = GPR, 2 = SIMD&FP, 0 = neither. */
static int
reg_class(reg_id_t r, int *num)
{
    if (r >= DR_REG_X0 && r <= DR_REG_X30) {
        *num = r - DR_REG_X0;
        return 1;
    }
    if (r >= DR_REG_W0 && r <= DR_REG_W30) {
        *num = r - DR_REG_W0;
        return 1;
    }
    static const reg_id_t vbase[] = { DR_REG_Q0, DR_REG_D0, DR_REG_S0, DR_REG_H0,
                                      DR_REG_B0 };
    for (int i = 0; i < 5; i++) {
        if (r >= vbase[i] && r <= vbase[i] + 31) {
            *num = r - vbase[i];
            return 2;
        }
    }
    return 0;
}

static reg_id_t
perm_reg(const perm_t *p, reg_id_t r)
{
    int n;
    switch (reg_class(r, &n)) {
    case 1: return r - n + p->gpr[n];
    case 2: return r - n + p->vec[n];
    default: return r;
    }
}

/* The family register of r for the resize APIs: X for GPRs, Q for SIMD&FP. */
static reg_id_t
family_reg(int cls, int n)
{
    return cls == 1 ? DR_REG_X0 + n : DR_REG_Q0 + n;
}

typedef struct {
    uint32_t gpr_used, vec_used; /* bit n: number n appears */
    bool has_list;               /* a consecutive register list */
    uint32_t gpr_fixed;          /* GPRs the encoding implies: not renamed */
    bool no_rename;              /* a GPR block the permutations cannot keep */
    bool other_regs;             /* fixed registers (SP, ZR, system) */
    int nregs;
    reg_id_t regs[64]; /* distinct register ids used */
} usage_t;

static void
note_reg(usage_t *u, reg_id_t r)
{
    int n;
    int c = reg_class(r, &n);
    if (c == 1)
        u->gpr_used |= 1U << n;
    else if (c == 2)
        u->vec_used |= 1U << n;
    else
        u->other_regs = true;
    for (int i = 0; i < u->nregs; i++) {
        if (u->regs[i] == r)
            return;
    }
    if (u->nregs < 64)
        u->regs[u->nregs++] = r;
}

/* Instructions whose SIMD&FP registers form a consecutive list. */
static bool
has_vector_list(int opc)
{
    switch (opc) {
    case OP_ld1:
    case OP_ld2:
    case OP_ld3:
    case OP_ld4:
    case OP_ld1r:
    case OP_ld2r:
    case OP_ld3r:
    case OP_ld4r:
    case OP_st1:
    case OP_st2:
    case OP_st3:
    case OP_st4:
    case OP_tbl:
    case OP_tbx: return true;
    default: return false;
    }
}

static void
get_usage(instr_t *in, usage_t *u)
{
    memset(u, 0, sizeof(*u));
    int opc = instr_get_opcode(in);
    u->has_list = has_vector_list(opc);
    /* PACIA1716 and friends use X16/X17 implicitly. */
    if (opc == OP_pacia1716 || opc == OP_pacib1716 || opc == OP_autia1716 ||
        opc == OP_autib1716)
        u->gpr_fixed |= (1U << 16) | (1U << 17);
    /* LD64B/ST64B* transfer eight consecutive registers from an even one. */
    if (strcmp(decode_opcode_name(opc), "ld64b") == 0 ||
        strcmp(decode_opcode_name(opc), "st64b") == 0 ||
        strcmp(decode_opcode_name(opc), "st64bv") == 0 ||
        strcmp(decode_opcode_name(opc), "st64bv0") == 0)
        u->no_rename = true;
    for (int k = 0; k < 2; k++) {
        int n = k == 0 ? instr_num_srcs(in) : instr_num_dsts(in);
        for (int i = 0; i < n; i++) {
            opnd_t o = k == 0 ? instr_get_src(in, i) : instr_get_dst(in, i);
            if (opnd_is_reg(o) && (opnd_get_flags(o) & DR_OPND_IMPLICIT) != 0)
                u->has_list = true;
            for (int j = 0; j < opnd_num_regs_used(o); j++)
                note_reg(u, opnd_get_reg_used(o, j));
        }
    }
}

/* Picks a collision-free permutation: no used register maps onto another
 * used register, so applying it one register at a time is the same as
 * applying it simultaneously.  Returns false if none was found.
 */
static bool
pick_perm(rng_t *r, const usage_t *u, perm_t *p)
{
    for (int tries = 0; tries < 64; tries++) {
        perm_identity(p);
        /* Shuffle the pairs that hold no fixed register. */
        int free_pairs[15], nfree = 0;
        for (int i = 0; i < 15; i++) {
            if ((u->gpr_fixed & (3U << (2 * i))) == 0)
                free_pairs[nfree++] = i;
        }
        int pairs[15];
        for (int i = 0; i < 15; i++)
            pairs[i] = i;
        for (int i = nfree - 1; i > 0; i--) {
            int j = (int)rnd_n(r, i + 1);
            int t = pairs[free_pairs[i]];
            pairs[free_pairs[i]] = pairs[free_pairs[j]];
            pairs[free_pairs[j]] = t;
        }
        for (int i = 0; i < 15; i++) {
            p->gpr[2 * i] = 2 * pairs[i];
            p->gpr[2 * i + 1] = 2 * pairs[i] + 1;
        }
        int k = 1 + (int)rnd_n(r, u->has_list ? 31 : 15);
        for (int i = 0; i < 32; i++)
            p->vec[i] = u->has_list ? (i + k) % 32 : (i & 16) | ((i + k) & 15);
        bool ok = true;
        for (int i = 0; i < 30 && ok; i++) {
            if ((u->gpr_used & (1U << i)) != 0 && p->gpr[i] != i &&
                (u->gpr_used & (1U << p->gpr[i])) != 0)
                ok = false;
        }
        for (int i = 0; i < 32 && ok; i++) {
            if ((u->vec_used & (1U << i)) != 0 && (u->vec_used & (1U << p->vec[i])) != 0)
                ok = false;
        }
        if (ok)
            return true;
    }
    return false;
}

/* Applies p to in through one of the DR APIs. */
static void
apply_perm(instr_t *in, const perm_t *p, const usage_t *u, int api)
{
    if (api == 1) {
        /* instr_replace_reg_resize() with each family's full-width register. */
        for (int n = 0; n < 31; n++) {
            if ((u->gpr_used & (1U << n)) != 0 && p->gpr[n] != n)
                instr_replace_reg_resize(in, family_reg(1, n), family_reg(1, p->gpr[n]));
        }
        for (int n = 0; n < 32; n++) {
            if ((u->vec_used & (1U << n)) != 0 && p->vec[n] != n)
                instr_replace_reg_resize(in, family_reg(2, n), family_reg(2, p->vec[n]));
        }
        return;
    }
    for (int k = 0; k < 2; k++) {
        int nops = k == 0 ? instr_num_srcs(in) : instr_num_dsts(in);
        for (int i = 0; i < nops; i++) {
            opnd_t o = k == 0 ? instr_get_src(in, i) : instr_get_dst(in, i);
            bool changed = false;
            if (api == 0) {
                /* opnd_replace_reg() with each exact register id. */
                for (int j = 0; j < u->nregs; j++) {
                    reg_id_t r = u->regs[j], nr = perm_reg(p, r);
                    if (nr != r && opnd_replace_reg(&o, r, nr))
                        changed = true;
                }
            } else {
                /* opnd_replace_reg_resize() with each family register. */
                for (int c = 1; c <= 2; c++) {
                    uint32_t used = c == 1 ? u->gpr_used : u->vec_used;
                    for (int n = 0; n < 32; n++) {
                        if ((used & (1U << n)) == 0)
                            continue;
                        int m = c == 1 ? p->gpr[n] : p->vec[n];
                        if (m != n &&
                            opnd_replace_reg_resize(&o, family_reg(c, n),
                                                    family_reg(c, m)))
                            changed = true;
                    }
                }
            }
            if (changed) {
                if (k == 0)
                    instr_set_src(in, i, o);
                else
                    instr_set_dst(in, i, o);
            }
        }
    }
}

/***************************************************************************
 * Query signatures: everything a client asks an instruction, over a register
 * universe that a permutation maps onto itself.
 */
#define MAX_UNIV 96
typedef struct {
    int opc, nsrc, ndst, len, pred;
    uint cat, efl_d, efl_a, ar_d, ar_a;
    uint bools;
    ptr_int_t movc;
    int nmem;
    uint memsz[8];
    int nu;
    reg_id_t u[MAX_UNIV];
    byte q[MAX_UNIV];
} qsig_t;

static const reg_id_t fixed_regs[] = { DR_REG_XZR,  DR_REG_WZR,  DR_REG_XSP, DR_REG_WSP,
                                       DR_REG_NZCV, DR_REG_FPCR, DR_REG_FPSR };

/* The universe for in, mapped through p (identity for the original). */
static int
make_universe(const usage_t *u, const perm_t *p, reg_id_t *univ)
{
    int n = 0;
    for (size_t i = 0; i < sizeof(fixed_regs) / sizeof(fixed_regs[0]); i++)
        univ[n++] = fixed_regs[i];
    for (int k = 0; k < 31; k++) {
        if ((u->gpr_used & (1U << k)) != 0) {
            univ[n++] = perm_reg(p, DR_REG_X0 + k);
            univ[n++] = perm_reg(p, DR_REG_W0 + k);
        }
    }
    static const reg_id_t vbase[] = { DR_REG_Q0, DR_REG_D0, DR_REG_S0, DR_REG_H0,
                                      DR_REG_B0 };
    for (int k = 0; k < 32 && n + 5 <= MAX_UNIV - 8; k++) {
        if ((u->vec_used & (1U << k)) != 0) {
            for (int v = 0; v < 5; v++)
                univ[n++] = perm_reg(p, vbase[v] + k);
        }
    }
    for (int i = 0; i < u->nregs && n < MAX_UNIV; i++) {
        int num;
        if (reg_class(u->regs[i], &num) == 0) {
            bool dup = false;
            for (int j = 0; j < n; j++)
                dup = dup || univ[j] == u->regs[i];
            if (!dup)
                univ[n++] = u->regs[i];
        }
    }
    return n;
}

static void
compute_sig(instr_t *in, const reg_id_t *univ, int nu, qsig_t *s)
{
    memset(s, 0, sizeof(*s));
    s->opc = instr_get_opcode(in);
    s->nsrc = instr_num_srcs(in);
    s->ndst = instr_num_dsts(in);
    s->len = instr_length(GD, in);
    s->pred = instr_get_predicate(in);
    s->cat = instr_get_category(in);
    s->efl_d = instr_get_eflags(in, DR_QUERY_DEFAULT);
    s->efl_a = instr_get_eflags(in, DR_QUERY_INCLUDE_ALL);
    s->ar_d = instr_get_arith_flags(in, DR_QUERY_DEFAULT);
    s->ar_a = instr_get_arith_flags(in, DR_QUERY_INCLUDE_ALL);
    bool b[] = {
        instr_reads_memory(in),
        instr_writes_memory(in),
        instr_is_cti(in),
        instr_is_cbr(in),
        instr_is_ubr(in),
        instr_is_mbr(in),
        instr_is_call(in),
        instr_is_call_direct(in),
        instr_is_call_indirect(in),
        instr_is_return(in),
        instr_is_predicated(in),
        instr_is_nop(in),
        instr_is_exclusive_load(in),
        instr_is_exclusive_store(in),
        instr_is_mov_constant(in, &s->movc),
    };
    for (size_t i = 0; i < sizeof(b) / sizeof(b[0]); i++)
        s->bools |= (b[i] ? 1U : 0U) << i;
    for (int k = 0; k < 2; k++) {
        int n = k == 0 ? s->nsrc : s->ndst;
        for (int i = 0; i < n && s->nmem < 8; i++) {
            opnd_t o = k == 0 ? instr_get_src(in, i) : instr_get_dst(in, i);
            if (opnd_is_memory_reference(o))
                s->memsz[s->nmem++] = opnd_size_in_bytes(opnd_get_size(o)) * 2 + k;
        }
    }
    s->nu = nu;
    for (int i = 0; i < nu; i++) {
        reg_id_t r = univ[i];
        s->u[i] = r;
        s->q[i] = (instr_reads_from_reg(in, r, DR_QUERY_DEFAULT) ? 1 : 0) |
            (instr_reads_from_reg(in, r, DR_QUERY_INCLUDE_ALL) ? 2 : 0) |
            (instr_writes_to_reg(in, r, DR_QUERY_DEFAULT) ? 4 : 0) |
            (instr_writes_to_reg(in, r, DR_QUERY_INCLUDE_ALL) ? 8 : 0) |
            (instr_reads_from_exact_reg(in, r, DR_QUERY_INCLUDE_ALL) ? 16 : 0) |
            (instr_writes_to_exact_reg(in, r, DR_QUERY_INCLUDE_ALL) ? 32 : 0) |
            (instr_uses_reg(in, r) ? 64 : 0);
    }
}

/* Compares a with b; b's universe is a's mapped through a permutation. */
static bool
sig_same(const qsig_t *a, const qsig_t *b, char *why, size_t whylen)
{
    if (a->opc != b->opc || a->nsrc != b->nsrc || a->ndst != b->ndst)
        WHY("opcode/operand counts");
    if (a->len != b->len)
        WHY("length %d vs %d", a->len, b->len);
    if (a->pred != b->pred)
        WHY("predicate");
    if (a->cat != b->cat)
        WHY("category 0x%x vs 0x%x", a->cat, b->cat);
    if (a->efl_d != b->efl_d || a->efl_a != b->efl_a)
        WHY("eflags 0x%x/0x%x vs 0x%x/0x%x", a->efl_d, a->efl_a, b->efl_d, b->efl_a);
    if (a->ar_d != b->ar_d || a->ar_a != b->ar_a)
        WHY("arith flags");
    if (a->bools != b->bools)
        WHY("boolean queries 0x%x vs 0x%x", a->bools, b->bools);
    if (a->movc != b->movc)
        WHY("mov constant");
    if (a->nmem != b->nmem || memcmp(a->memsz, b->memsz, sizeof(a->memsz)) != 0)
        WHY("memory operand sizes");
    if (a->nu != b->nu)
        WHY("universe size");
    for (int i = 0; i < a->nu; i++) {
        if (a->q[i] != b->q[i]) {
            WHY("register queries for %s (0x%x) vs %s (0x%x)", get_register_name(a->u[i]),
                a->q[i], get_register_name(b->u[i]), b->q[i]);
        }
    }
    return true;
}

static void
sig_of(instr_t *in, const usage_t *u, const perm_t *p, qsig_t *s)
{
    reg_id_t univ[MAX_UNIV];
    int nu = make_universe(u, p, univ);
    compute_sig(in, univ, nu, s);
}

/***************************************************************************
 * The regdeps projection model.
 *
 * instr_convert_to_isa_regdeps() walks the operands; the model asks the
 * register queries instead and maps registers to families of its own
 * (W/X n -> GPR n, B/H/S/D/Q n -> V n).  The family -> virtual register map
 * is learned (an instruction with one unknown family and one unknown virtual
 * register on the same side fixes the pair) and must stay a bijection.
 */
#define FAM_ZR 31
#define FAM_SP 32
#define FAM_V 40
#define FAM_OTHER 100
#define MAX_FAM (FAM_OTHER + 1024)
static int fam_to_virt[MAX_FAM]; /* 0 = unknown */
static int virt_to_fam[1024];    /* -1 = unknown */

static int
reg_fam(reg_id_t r)
{
    int n;
    switch (reg_class(r, &n)) {
    case 1: return n;
    case 2: return FAM_V + n;
    default: break;
    }
    if (r == DR_REG_XZR || r == DR_REG_WZR)
        return FAM_ZR;
    if (r == DR_REG_XSP || r == DR_REG_WSP)
        return FAM_SP;
    return FAM_OTHER + (r < 1024 ? r : 1023);
}

typedef struct {
    int nsrc, ndst;
    int src[64], dst[64];
} famset_t;

static void
add_fam(int *set, int *n, int f)
{
    for (int i = 0; i < *n; i++) {
        if (set[i] == f)
            return;
    }
    if (*n < 64)
        set[(*n)++] = f;
}

static void
model_regdeps(instr_t *in, const usage_t *u, famset_t *m)
{
    m->nsrc = m->ndst = 0;
    for (int i = 0; i < u->nregs; i++) {
        reg_id_t r = u->regs[i];
        if (instr_reads_from_reg(in, r, DR_QUERY_INCLUDE_ALL))
            add_fam(m->src, &m->nsrc, reg_fam(r));
        if (instr_writes_to_reg(in, r, DR_QUERY_INCLUDE_ALL))
            add_fam(m->dst, &m->ndst, reg_fam(r));
    }
}

static void
regdeps_sets(instr_t *rd, famset_t *v)
{
    v->nsrc = v->ndst = 0;
    for (int i = 0; i < instr_num_srcs(rd); i++)
        add_fam(v->src, &v->nsrc, opnd_get_reg(instr_get_src(rd, i)));
    for (int i = 0; i < instr_num_dsts(rd); i++)
        add_fam(v->dst, &v->ndst, opnd_get_reg(instr_get_dst(rd, i)));
}

static bool
in_set(const int *set, int n, int x)
{
    for (int i = 0; i < n; i++) {
        if (set[i] == x)
            return true;
    }
    return false;
}

/* Learns from one side; returns false on a conflict. */
static bool
learn_side(const int *fams, int nf, const int *virts, int nv, char *why, size_t whylen)
{
    int uf = -1, uv = -1, nuf = 0, nuv = 0;
    for (int i = 0; i < nf; i++) {
        int f = fams[i];
        if (fam_to_virt[f] == 0) {
            uf = f;
            nuf++;
        } else if (!in_set(virts, nv, fam_to_virt[f])) {
            WHY("family %d maps to virtual register %d, not in the projection", f,
                fam_to_virt[f]);
        }
    }
    for (int i = 0; i < nv; i++) {
        int v = virts[i];
        if (v < 0 || v >= 1024)
            WHY("virtual register %d out of range", v);
        if (virt_to_fam[v] < 0) {
            uv = v;
            nuv++;
        } else if (!in_set(fams, nf, virt_to_fam[v])) {
            WHY("virtual register %s is family %d, which the model does not have",
                get_register_name((reg_id_t)v), virt_to_fam[v]);
        }
    }
    if (nuf == 1 && nuv == 1) {
        fam_to_virt[uf] = uv;
        virt_to_fam[uv] = uf;
        n_regdeps_learned++;
    }
    return true;
}

static bool
regdeps_mapped_same(const int *fams, int nf, const int *virts, int nv)
{
    if (nf != nv)
        return false;
    for (int i = 0; i < nf; i++) {
        if (fam_to_virt[fams[i]] == 0 || !in_set(virts, nv, fam_to_virt[fams[i]]))
            return false;
    }
    return true;
}

static bool
all_known(const famset_t *m)
{
    for (int i = 0; i < m->nsrc; i++) {
        if (fam_to_virt[m->src[i]] == 0)
            return false;
    }
    for (int i = 0; i < m->ndst; i++) {
        if (fam_to_virt[m->dst[i]] == 0)
            return false;
    }
    return true;
}

typedef struct {
    famset_t v; /* the projection's virtual registers */
    uint arith;
    uint cat;
    opnd_size_t opsz;
} proj_t;

/* Projects in; checks the projection against the model (if check) and its
 * own encode/decode round trip.  Returns false if the projection failed.
 */
static bool
regdeps_check(instr_t *in, const usage_t *u, bool check, proj_t *out)
{
    char why[256];
    instr_t *rd = instr_create(GD);
    instr_convert_to_isa_regdeps(GD, in, rd);
    regdeps_sets(rd, &out->v);
    out->arith = instr_get_arith_flags(rd, DR_QUERY_DEFAULT);
    out->cat = instr_get_category(rd);
    out->opsz = instr_get_operation_size(rd);
    bool ok = true;
    if (check) {
        famset_t m;
        model_regdeps(in, u, &m);
        if (instr_get_isa_mode(rd) != DR_ISA_REGDEPS ||
            instr_get_opcode(rd) != OP_UNDECODED) {
            report(P_REGDEPS, "projection is not an undecoded regdeps instruction");
            ok = false;
        }
        if (ok &&
            (!learn_side(m.src, m.nsrc, out->v.src, out->v.nsrc, why, sizeof(why)) ||
             !learn_side(m.dst, m.ndst, out->v.dst, out->v.ndst, why, sizeof(why)))) {
            report(P_REGDEPS, "%s", why);
            ok = false;
        }
        if (ok && (m.nsrc != out->v.nsrc || m.ndst != out->v.ndst)) {
            report(P_REGDEPS, "model has %d sources/%d destinations, projection %d/%d",
                   m.nsrc, m.ndst, out->v.nsrc, out->v.ndst);
            ok = false;
        }
        if (ok && all_known(&m) &&
            (!regdeps_mapped_same(m.src, m.nsrc, out->v.src, out->v.nsrc) ||
             !regdeps_mapped_same(m.dst, m.ndst, out->v.dst, out->v.ndst))) {
            report(P_REGDEPS, "projected registers differ from the model");
            ok = false;
        } else if (ok && !all_known(&m))
            n_regdeps_unmapped++;
        uint want = instr_get_arith_flags(in, DR_QUERY_DEFAULT);
        uint want_rd = ((want & EFLAGS_READ_ARITH) != 0 ? EFLAGS_READ_ARITH : 0) |
            ((want & EFLAGS_WRITE_ARITH) != 0 ? EFLAGS_WRITE_ARITH : 0);
        if (ok && out->arith != want_rd) {
            report(P_REGDEPS, "arithmetic flags 0x%x, the instruction's are 0x%x",
                   out->arith, want);
            ok = false;
        }
        if (ok && out->cat != instr_get_category(in)) {
            report(P_REGDEPS, "category 0x%x vs 0x%x", out->cat, instr_get_category(in));
            ok = false;
        }
        /* The projection's own encoding round trip. */
        byte ALIGN_VAR(8) buf[32];
        memset(buf, 0, sizeof(buf));
        byte *end = instr_encode(GD, rd, buf);
        if (ok && end == NULL && out->v.nsrc + out->v.ndst > 8) {
            known(P_REGDEPS, K_REGDEPS_OPNDS, "%d registers", out->v.nsrc + out->v.ndst);
        } else if (ok && end == NULL) {
            report(P_REGDEPS, "projection does not encode");
            ok = false;
        } else if (ok) {
            instr_t *back = instr_create(GD);
            dr_isa_mode_t old;
            dr_set_isa_mode(GD, DR_ISA_REGDEPS, &old);
            byte *dend = decode(GD, buf, back);
            dr_set_isa_mode(GD, old, NULL);
            famset_t bv;
            if (dend != end) {
                report(P_REGDEPS, "projection decodes to %d bytes, encoded %d",
                       dend == NULL ? -1 : (int)(dend - buf), (int)(end - buf));
                ok = false;
            } else {
                regdeps_sets(back, &bv);
                if (bv.nsrc != out->v.nsrc || bv.ndst != out->v.ndst ||
                    memcmp(bv.src, out->v.src, sizeof(int) * bv.nsrc) != 0 ||
                    memcmp(bv.dst, out->v.dst, sizeof(int) * bv.ndst) != 0 ||
                    instr_get_arith_flags(back, DR_QUERY_DEFAULT) != out->arith ||
                    instr_get_category(back) != out->cat ||
                    instr_get_operation_size(back) != out->opsz) {
                    report(P_REGDEPS, "projection's encode/decode round trip differs");
                    ok = false;
                }
            }
            instr_destroy(GD, back);
        }
        checked[P_REGDEPS]++;
    }
    instr_destroy(GD, rd);
    return ok;
}

static bool
proj_same(const proj_t *a, const proj_t *b)
{
    if (a->v.nsrc != b->v.nsrc || a->v.ndst != b->v.ndst || a->arith != b->arith ||
        a->cat != b->cat || a->opsz != b->opsz)
        return false;
    for (int i = 0; i < a->v.nsrc; i++) {
        if (!in_set(b->v.src, b->v.nsrc, a->v.src[i]))
            return false;
    }
    for (int i = 0; i < a->v.ndst; i++) {
        if (!in_set(b->v.dst, b->v.ndst, a->v.dst[i]))
            return false;
    }
    return true;
}

/* The projection of a renamed instruction is the projection mapped by the
 * permutation (through the learned family map).  -1: cannot tell (unknown
 * families), 0: differs, 1: same.
 */
static int
proj_renamed_same(const proj_t *a, const proj_t *b, const perm_t *p)
{
    for (int side = 0; side < 2; side++) {
        const int *av = side == 0 ? a->v.src : a->v.dst;
        int an = side == 0 ? a->v.nsrc : a->v.ndst;
        const int *bv = side == 0 ? b->v.src : b->v.dst;
        int bn = side == 0 ? b->v.nsrc : b->v.ndst;
        if (an != bn)
            return 0;
        for (int i = 0; i < an; i++) {
            if (av[i] < 0 || av[i] >= 1024 || virt_to_fam[av[i]] < 0)
                return -1;
            int f = virt_to_fam[av[i]], g = f;
            if (f < 31)
                g = p->gpr[f];
            else if (f >= FAM_V && f < FAM_V + 32)
                g = FAM_V + p->vec[f - FAM_V];
            if (fam_to_virt[g] == 0)
                return -1;
            if (!in_set(bv, bn, fam_to_virt[g]))
                return 0;
        }
    }
    return a->arith == b->arith && a->cat == b->cat && a->opsz == b->opsz ? 1 : 0;
}

/***************************************************************************
 * PC-relative instructions and the relocation model.
 */
typedef struct {
    bool pcrel;
    int idx;      /* source index of the target */
    uint64_t tgt; /* absolute target */
    int bits;     /* signed offset field width */
    int scale;    /* log2 of the offset unit: 2 words, 0 bytes, 12 pages */
    bool known;   /* opcode is in the model */
} pcrel_t;

static void
get_pcrel(instr_t *in, pcrel_t *pr)
{
    memset(pr, 0, sizeof(*pr));
    for (int i = 0; i < instr_num_srcs(in); i++) {
        opnd_t o = instr_get_src(in, i);
        if (opnd_is_pc(o)) {
            pr->tgt = (uint64_t)opnd_get_pc(o);
        } else if (opnd_is_rel_addr(o)) {
            pr->tgt = (uint64_t)opnd_get_addr(o);
        } else
            continue;
        pr->pcrel = true;
        pr->idx = i;
        break;
    }
    if (!pr->pcrel)
        return;
    pr->known = true;
    switch (instr_get_opcode(in)) {
    case OP_b:
    case OP_bl:
        pr->bits = 26;
        pr->scale = 2;
        break;
    case OP_tbz:
    case OP_tbnz:
        pr->bits = 14;
        pr->scale = 2;
        break;
    case OP_adr:
        pr->bits = 21;
        pr->scale = 0;
        break;
    case OP_adrp:
        pr->bits = 21;
        pr->scale = 12;
        break;
    case OP_bcond:
    case OP_cbz:
    case OP_cbnz:
    case OP_ldr:
    case OP_ldrsw:
    case OP_prfm:
        pr->bits = 19;
        pr->scale = 2;
        break;
    default:
        pr->known = false;
        pr->bits = 19;
        pr->scale = 2;
    }
}

/* Whether the target is reachable from pc. */
static bool
pcrel_representable(const pcrel_t *pr, uint64_t pc)
{
    int64_t off;
    if (pr->scale == 12)
        off = (int64_t)((pr->tgt & ~0xfffULL) - (pc & ~0xfffULL)) >> 12;
    else {
        off = (int64_t)(pr->tgt - pc);
        if ((off & ((1 << pr->scale) - 1)) != 0)
            return false;
        off >>= pr->scale;
    }
    return off >= -(1LL << (pr->bits - 1)) && off < (1LL << (pr->bits - 1));
}

static uint64_t
pcrel_target_of(opnd_t o)
{
    if (opnd_is_pc(o))
        return (uint64_t)opnd_get_pc(o);
    if (opnd_is_rel_addr(o))
        return (uint64_t)opnd_get_addr(o);
    return 0;
}

/***************************************************************************
 * The properties of one word.
 */
typedef struct {
    uint32_t word, alt; /* alt: listed canonical encoding, 0 if none */
    bool vector;        /* from the vectors (else canonical = fixed point) */
} input_t;

static const int64_t reloc_deltas[] = {
    4,
    -4,
    4096,
    -65536,
    (1 << 15) - 4,
    1 << 15,
    -(1 << 15) - 4,
    (1 << 20) - 4,
    1 << 20,
    -(1 << 20) - 8,
    1 << 21,
    (1LL << 27) - 4,
    1LL << 27,
    -(1LL << 27) - 4,
    3LL << 30,
    5LL << 30,
    -(5LL << 30),
    0x123456788LL,
};
#define NUM_DELTAS (sizeof(reloc_deltas) / sizeof(reloc_deltas[0]))

static int g_slot_next;

static int
next_slot(void)
{
    int s = g_slot_next;
    g_slot_next = (g_slot_next + 1) % SLOT_ILIST;
    return s;
}

/* canon: returns the canonical word (and false if there is none). */
static bool
prop_canon(const input_t *inp, uint32_t *canon)
{
    char why[256];
    instr_t *in = dec_copy(inp->word, P_BASE);
    if (in == NULL)
        return false;
    uint32_t e;
    bool ok = enc_at(in, P_BASE, &e);
    if (prop_on[P_CANON])
        checked[P_CANON]++;
    if (!ok) {
        if (prop_on[P_CANON])
            report(P_CANON, "decoded instruction does not encode");
        instr_destroy(GD, in);
        return false;
    }
    bool good = true;
    if (inp->vector) {
        if (e != inp->word && (inp->alt == 0 || e != inp->alt)) {
            if (prop_on[P_CANON])
                report(P_CANON,
                       "re-encodes as 0x%08x, not the word or its canonical form", e);
            good = false;
        }
    }
    /* Either way, the encoding decodes back to the same IR and is a fixed point. */
    instr_t *back = dec_copy(e, P_BASE);
    uint32_t e2 = 0;
    if (back == NULL || !instr_strict(in, back, false, false, why, sizeof(why)) ||
        !enc_at(back, P_BASE, &e2) || e2 != e) {
        if (prop_on[P_CANON] && good) {
            report(P_CANON,
                   "re-encoding 0x%08x is not a fixed point (%s, re-encodes as "
                   "0x%08x)",
                   e, back == NULL ? "does not decode" : why, e2);
        }
        good = false;
    }
    if (back != NULL)
        instr_destroy(GD, back);
    instr_destroy(GD, in);
    *canon = e;
    return good;
}

static void
prop_clone(const input_t *inp, uint32_t canon, rng_t *r)
{
    char why[256];
    bool inplace = rnd_n(r, 2) == 0;
    int slot = next_slot();
    instr_t *in = inplace ? dec_inplace(inp->word, slot) : dec_copy(inp->word, P_BASE);
    if (in == NULL)
        return;
    uint64_t pc = inplace ? (uint64_t)&g_slots[slot] : P_BASE;
    n_clone_inplace += inplace;
    usage_t u;
    perm_t id;
    perm_identity(&id);
    get_usage(in, &u);
    qsig_t s0, s1;
    sig_of(in, &u, &id, &s0);
    instr_t *c = instr_clone(GD, in);
    checked[P_CLONE]++;
    sig_of(c, &u, &id, &s1);
    if (!instr_strict(in, c, false, false, why, sizeof(why)) || !instr_same(in, c)) {
        report(P_CLONE, "clone differs: %s", why);
    } else if (!sig_same(&s0, &s1, why, sizeof(why))) {
        report(P_CLONE, "clone's queries differ: %s", why);
    } else if (instr_raw_bits_valid(c) != instr_raw_bits_valid(in) ||
               instr_get_app_pc(c) != instr_get_app_pc(in)) {
        report(P_CLONE, "clone's raw bits or translation differ");
    }
    proj_t p0, p1;
    bool rd0 = prop_on[P_REGDEPS] && regdeps_check(in, &u, false, &p0);
    /* Free the original, and overwrite the bytes it was decoded from. */
    instr_destroy(GD, in);
    if (inplace)
        g_slots[slot] = 0xd503201f; /* nop */
    sig_of(c, &u, &id, &s1);
    uint32_t e;
    if (!sig_same(&s0, &s1, why, sizeof(why)))
        report(P_CLONE, "after freeing the original, the clone's queries differ: %s",
               why);
    else if (!enc_at(c, pc, &e))
        report(P_CLONE, "after freeing the original, the clone does not encode");
    else if (e != canon && e != inp->word)
        report(P_CLONE, "after freeing the original, the clone encodes as 0x%08x", e);
    if (rd0 && regdeps_check(c, &u, false, &p1) && !proj_same(&p0, &p1))
        report(P_REGDEPS, "projection of a clone differs");
    instr_destroy(GD, c);
}

static void
prop_rawbits(const input_t *inp, uint32_t canon, rng_t *r)
{
    char why[256];
    int slot = next_slot();
    instr_t *in = dec_inplace(inp->word, slot);
    if (in == NULL)
        return;
    uint64_t pc = (uint64_t)&g_slots[slot];
    instr_t *ref = dec_copy(inp->word, pc);
    usage_t u;
    perm_t id;
    perm_identity(&id);
    get_usage(in, &u);
    qsig_t s0, s1;
    sig_of(in, &u, &id, &s0);
    checked[P_RAWBITS]++;
    uint32_t e;
    if (!instr_raw_bits_valid(in)) {
        report(P_RAWBITS, "an instruction decoded in place has no valid raw bits");
        goto done;
    }
    /* A new translation is attribution only. */
    app_pc tr = (app_pc)(P_BASE + 4 * rnd_n(r, 1 << 20));
    instr_set_translation(in, tr);
    sig_of(in, &u, &id, &s1);
    if (instr_get_app_pc(in) != tr) {
        report(P_RAWBITS, "instr_set_translation() did not set the translation");
        goto done;
    }
    if (!instr_raw_bits_valid(in)) {
        report(P_RAWBITS, "instr_set_translation() invalidated the raw bits");
        goto done;
    }
    if (!sig_same(&s0, &s1, why, sizeof(why))) {
        report(P_RAWBITS, "instr_set_translation() changed the queries: %s", why);
        goto done;
    }
    if (!enc_at(in, pc, &e) || e != inp->word) {
        report(P_RAWBITS, "with valid raw bits, encodes as 0x%08x, not the raw word", e);
        goto done;
    }
    /* Invalidated raw bits: the instruction is re-encoded, canonically. */
    instr_set_raw_bits_valid(in, false);
    sig_of(in, &u, &id, &s1);
    if (instr_get_app_pc(in) != tr) {
        report(P_RAWBITS, "invalidating the raw bits changed the translation");
        goto done;
    }
    if (!sig_same(&s0, &s1, why, sizeof(why))) {
        report(P_RAWBITS, "invalidating the raw bits changed the queries: %s", why);
        goto done;
    }
    if (!enc_at(in, pc, &e) || e != canon) {
        report(P_RAWBITS, "with invalid raw bits, encodes as 0x%08x, not 0x%08x", e,
               canon);
        goto done;
    }
    if (ref != NULL && !instr_strict(in, ref, false, false, why, sizeof(why))) {
        report(P_RAWBITS, "differs from a fresh decode: %s", why);
        goto done;
    }
    /* Setting an operand (even to itself) must invalidate the raw bits. */
    instr_destroy(GD, in);
    in = dec_inplace(inp->word, slot);
    if (in != NULL && instr_num_srcs(in) + instr_num_dsts(in) > 0) {
        if (instr_num_srcs(in) > 0)
            instr_set_src(in, 0, instr_get_src(in, 0));
        else
            instr_set_dst(in, 0, instr_get_dst(in, 0));
        if (instr_raw_bits_valid(in))
            report(P_RAWBITS, "setting an operand left the raw bits valid");
    }
done:
    if (in != NULL)
        instr_destroy(GD, in);
    if (ref != NULL)
        instr_destroy(GD, ref);
}

static void
reloc_one(instr_t *in, const pcrel_t *pr, uint64_t pc, int64_t delta, uint32_t raw,
          uint32_t canon, bool rawvalid)
{
    char why[256];
    uint64_t np = pc + delta;
    uint32_t e = 0;
    bool ok = enc_at(in, np, &e);
    checked[P_RELOC]++;
    if (!pr->pcrel) {
        if (!ok)
            report(P_RELOC, "does not encode at pc+0x%llx", (long long)delta);
        else if (e != canon && !(rawvalid && e == raw))
            report(P_RELOC, "encodes as 0x%08x at pc+0x%llx, not 0x%08x", e,
                   (long long)delta, canon);
        return;
    }
    n_reloc_pcrel++;
    bool rep = pcrel_representable(pr, np);
    if (rawvalid && ok && e == raw && delta != 0) {
        /* The raw bytes were copied: the target moved with the instruction. */
        instr_t *at = dec_copy(e, np);
        if (at != NULL && pcrel_target_of(instr_get_src(at, pr->idx)) != pr->tgt) {
            known(P_RELOC, K_RAW_PCREL, "copied 0x%08x to pc+0x%llx", e,
                  (long long)delta);
            instr_destroy(GD, at);
            return;
        }
        if (at != NULL)
            instr_destroy(GD, at);
    }
    if (!rep) {
        n_reloc_rejected += !ok;
        if (ok) {
            report(P_RELOC,
                   "target 0x%llx is out of range from pc+0x%llx but encoded "
                   "as 0x%08x",
                   (unsigned long long)pr->tgt, (long long)delta, e);
        }
        return;
    }
    if (!ok) {
        report(P_RELOC,
               "target 0x%llx is in range from pc+0x%llx but the encoder "
               "rejected it",
               (unsigned long long)pr->tgt, (long long)delta);
        return;
    }
    instr_t *at = dec_copy(e, np);
    if (at == NULL) {
        report(P_RELOC, "encoding 0x%08x at pc+0x%llx does not decode", e,
               (long long)delta);
        return;
    }
    if (!instr_strict(in, at, false, false, why, sizeof(why))) {
        report(P_RELOC, "encoded at pc+0x%llx as 0x%08x, decodes differently: %s",
               (long long)delta, e, why);
    }
    instr_destroy(GD, at);
}

static void
prop_reloc(const input_t *inp, uint32_t canon, rng_t *r)
{
    instr_t *in = dec_copy(inp->word, P_BASE);
    if (in == NULL)
        return;
    pcrel_t pr;
    get_pcrel(in, &pr);
    if (pr.pcrel && !pr.known) {
        checked[P_RELOC]++;
        report(P_RELOC, "PC-relative opcode %s is not in the relocation model",
               decode_opcode_name(instr_get_opcode(in)));
        instr_destroy(GD, in);
        return;
    }
    /* Every delta for PC-relative instructions, three for the rest. */
    for (size_t i = 0; i < NUM_DELTAS; i++) {
        if (!pr.pcrel && rnd_n(r, NUM_DELTAS) >= 3)
            continue;
        reloc_one(in, &pr, P_BASE, reloc_deltas[i], inp->word, canon, false);
    }
    instr_destroy(GD, in);
    /* The same from an instruction whose raw bits are valid. */
    int slot = next_slot();
    in = dec_inplace(inp->word, slot);
    if (in == NULL)
        return;
    get_pcrel(in, &pr);
    for (size_t i = 0; i < NUM_DELTAS; i++) {
        if (rnd_n(r, NUM_DELTAS) >= (pr.pcrel ? 6 : 2))
            continue;
        reloc_one(in, &pr, (uint64_t)&g_slots[slot], reloc_deltas[i], inp->word, canon,
                  true);
    }
    instr_destroy(GD, in);
}

static void
prop_rename(const input_t *inp, uint32_t canon, rng_t *r)
{
    char why[256];
    bool inplace = rnd_n(r, 2) == 0;
    int slot = next_slot();
    instr_t *in = inplace ? dec_inplace(inp->word, slot) : dec_copy(inp->word, P_BASE);
    if (in == NULL)
        return;
    uint64_t pc = inplace ? (uint64_t)&g_slots[slot] : P_BASE;
    instr_t *orig = dec_copy(inp->word, pc);
    usage_t u;
    get_usage(in, &u);
    perm_t p, inv, id;
    perm_identity(&id);
    if ((u.gpr_used | u.vec_used) == 0 || u.no_rename || !pick_perm(r, &u, &p)) {
        n_rename_skipped++;
        goto done;
    }
    perm_inverse(&p, &inv);
    int api = (int)rnd_n(r, 3);
    n_rename_api[api]++;
    n_rename_inplace += inplace;
    checked[P_RENAME]++;
    qsig_t s0, s1;
    sig_of(in, &u, &id, &s0);
    proj_t p0, p1;
    bool rd0 = prop_on[P_REGDEPS] && regdeps_check(in, &u, false, &p0);
    apply_perm(in, &p, &u, api);
    static const char *const api_names[] = { "opnd_replace_reg",
                                             "instr_replace_reg_resize",
                                             "opnd_replace_reg_resize" };
    /* Exactly the requested occurrences changed. */
    for (int k = 0; k < 2; k++) {
        int n = k == 0 ? instr_num_srcs(in) : instr_num_dsts(in);
        for (int i = 0; i < n; i++) {
            opnd_t want = k == 0 ? instr_get_src(orig, i) : instr_get_dst(orig, i);
            opnd_t got = k == 0 ? instr_get_src(in, i) : instr_get_dst(in, i);
            if (!opnd_strict_mapped(want, got, false, why, sizeof(why),
                    opnd_is_reg(want) ? perm_reg(&p, opnd_get_reg(want)) : DR_REG_NULL,
                    opnd_is_base_disp(want) ? perm_reg(&p, opnd_get_base(want)) : DR_REG_NULL,
                    opnd_is_base_disp(want) ? perm_reg(&p, opnd_get_index(want)) : DR_REG_NULL)) {
                report(P_RENAME, "%s: %s %d after renaming: %s", api_names[api],
                       k == 0 ? "src" : "dst", i, why);
                goto done;
            }
        }
    }
    bool moved = false;
    for (int n = 0; n < 32; n++) {
        if (((u.gpr_used >> n) & 1) != 0 && p.gpr[n] != n)
            moved = true;
        if (((u.vec_used >> n) & 1) != 0 && p.vec[n] != n)
            moved = true;
    }
    if (instr_raw_bits_valid(in) && moved) {
        report(P_RENAME, "%s: raw bits still valid after renaming", api_names[api]);
        goto done;
    }
    /* Queries follow the registers. */
    sig_of(in, &u, &p, &s1);
    if (!sig_same(&s0, &s1, why, sizeof(why))) {
        report(P_RENAME, "%s: renamed instruction's queries differ: %s", api_names[api],
               why);
        goto done;
    }
    if (rd0 && regdeps_check(in, &u, false, &p1) && proj_renamed_same(&p0, &p1, &p) == 0)
        report(P_REGDEPS,
               "projection of a renamed instruction is not the renamed "
               "projection");
    /* The renamed instruction encodes, and decodes back to itself. */
    uint32_t e;
    if (!enc_at(in, pc, &e)) {
        char d[128];
        dis_instr(in, d, sizeof(d));
        report(P_RENAME, "%s: renamed instruction \"%s\" does not encode", api_names[api],
               d);
        goto done;
    }
    instr_t *back = dec_copy(e, pc);
    if (back == NULL || !instr_strict(in, back, false, false, why, sizeof(why))) {
        report(P_RENAME,
               "%s: renamed instruction encodes as 0x%08x, which decodes "
               "differently: %s",
               api_names[api], e, back == NULL ? "invalid" : why);
        if (back != NULL)
            instr_destroy(GD, back);
        goto done;
    }
    instr_destroy(GD, back);
    /* The inverse restores the original. */
    usage_t ur;
    get_usage(in, &ur);
    apply_perm(in, &inv, &ur, api);
    if (!instr_strict(orig, in, false, false, why, sizeof(why)) ||
        !instr_same(orig, in)) {
        report(P_RENAME,
               "%s: the inverse permutation does not restore the instruction: "
               "%s",
               api_names[api], why);
        goto done;
    }
    /* (If no used register moved, the raw bits are still valid and copied.) */
    if (!enc_at(in, pc, &e) || (e != canon && !(!moved && e == inp->word)))
        report(P_RENAME, "%s: restored instruction encodes as 0x%08x, not 0x%08x",
               api_names[api], e, canon);
done:
    instr_destroy(GD, in);
    if (orig != NULL)
        instr_destroy(GD, orig);
}

static void
prop_regdeps(const input_t *inp)
{
    instr_t *in = dec_copy(inp->word, P_BASE);
    if (in == NULL)
        return;
    usage_t u;
    get_usage(in, &u);
    proj_t pj;
    regdeps_check(in, &u, true, &pj);
    instr_destroy(GD, in);
}

/* Whether a and b are strictly the same but for the pre-index state of a
 * zero-offset memory operand.
 */
static bool
only_pre_index_differs(instr_t *a, instr_t *b)
{
    if (instr_num_srcs(a) != instr_num_srcs(b) || instr_num_dsts(a) != instr_num_dsts(b))
        return false;
    instr_t *c = instr_clone(GD, b);
    for (int k = 0; k < 2; k++) {
        int n = k == 0 ? instr_num_srcs(c) : instr_num_dsts(c);
        for (int i = 0; i < n; i++) {
            opnd_t oa = k == 0 ? instr_get_src(a, i) : instr_get_dst(a, i);
            opnd_t oc = k == 0 ? instr_get_src(c, i) : instr_get_dst(c, i);
            if (opnd_is_base_disp(oa) && opnd_is_base_disp(oc)) {
                if (opnd_get_disp(oa) != 0 || opnd_get_disp(oc) != 0 ||
                    opnd_get_base(oa) != opnd_get_base(oc) ||
                    opnd_get_index(oa) != opnd_get_index(oc) ||
                    opnd_get_size(oa) != opnd_get_size(oc) ||
                    opnd_get_flags(oa) != opnd_get_flags(oc) ||
                    opnd_get_vector_element_size(oa) != opnd_get_vector_element_size(oc)) {
                    instr_destroy(GD, c);
                    return false;
                }
                bool sa, sc;
                uint aa, ac;
                if (opnd_get_index(oa) != DR_REG_NULL &&
                    (opnd_get_index_extend(oa, &sa, &aa) !=
                     opnd_get_index_extend(oc, &sc, &ac) || sa != sc || aa != ac)) {
                    instr_destroy(GD, c);
                    return false;
                }
                oc = oa;
                if (k == 0)
                    instr_set_src(c, i, oc);
                else
                    instr_set_dst(c, i, oc);
            }
        }
    }
    bool res = instr_strict(a, c, false, false, NULL, 0);
    instr_destroy(GD, c);
    return res;
}

/* instr_same() of two decoded words agrees with their canonical encodings:
 * different encodings are different instructions, and the same encoding is
 * the same instruction (e.g., words that differ only in should-be-one bits).
 */
static void
prop_same(uint32_t w1, uint32_t w2)
{
    instr_t *a = dec_copy(w1, P_BASE), *b = dec_copy(w2, P_BASE);
    uint32_t c1, c2;
    if (a != NULL && b != NULL && instr_get_opcode(a) != OP_xx &&
        instr_get_opcode(b) != OP_xx && !uses_sve(a) && !uses_sve(b) &&
        !sve_or_sme_space(w1) && !sve_or_sme_space(w2) && enc_at(a, P_BASE, &c1) &&
        enc_at(b, P_BASE, &c2)) {
        checked[P_CANON]++;
        n_same_checked++;
        bool same = instr_same(a, b);
        if (c1 != c2 && same && only_pre_index_differs(a, b)) {
            /* Pre- and post-index with a zero offset are the same access and
             * writeback: instr_same() may call them the same.
             */
            n_same_prepost++;
        } else if (c1 != c2 && same) {
            char d[128];
            dis_instr(b, d, sizeof(d));
            report(P_CANON,
                   "instr_same() with 0x%08x \"%s\", which encodes differently "
                   "(0x%08x vs 0x%08x)",
                   w2, d, c1, c2);
        } else if (c1 == c2 && !same) {
            report(P_CANON,
                   "not instr_same() with 0x%08x, which has the same canonical "
                   "encoding 0x%08x",
                   w2, c1);
        }
    }
    if (a != NULL)
        instr_destroy(GD, a);
    if (b != NULL)
        instr_destroy(GD, b);
}

/* Runs every property on one input.  Returns false if it was not usable. */
static bool
run_word(const input_t *inp, uint64_t seed, const char *what)
{
    set_case(inp->word, seed, what);
    instr_t *in = dec_copy(inp->word, P_BASE);
    if (in == NULL)
        return false;
    int opc = instr_get_opcode(in);
    bool usable = opc != OP_xx && opc != OP_INVALID && opc != OP_UNDECODED &&
        !uses_sve(in) && !sve_or_sme_space(inp->word);
    instr_destroy(GD, in);
    if (!usable)
        return false;
    rng_t r = rng_for(seed, inp->word);
    uint32_t canon;
    if (!prop_canon(inp, &canon))
        return true;
    if (prop_on[P_CLONE])
        prop_clone(inp, canon, &r);
    if (prop_on[P_RAWBITS])
        prop_rawbits(inp, canon, &r);
    if (prop_on[P_RELOC])
        prop_reloc(inp, canon, &r);
    if (prop_on[P_RENAME])
        prop_rename(inp, canon, &r);
    if (prop_on[P_REGDEPS])
        prop_regdeps(inp);
    return true;
}

/***************************************************************************
 * Inputs: the test vectors and their mutants.
 */
static input_t *g_vec;
static int g_nvec;

static void
load_vectors(int nfiles, char **files)
{
    int cap = 1 << 16;
    g_vec = malloc(cap * sizeof(input_t));
    for (int f = 0; f < nfiles; f++) {
        if (strstr(files[f], "-sve") != NULL)
            continue; /* SVE is out of scope. */
        FILE *fp = fopen(files[f], "r");
        if (fp == NULL) {
            fprintf(stderr, "ERROR: cannot open %s\n", files[f]);
            exit(2);
        }
        char line[1024];
        while (fgets(line, sizeof(line), fp) != NULL) {
            char *end;
            unsigned long w = strtoul(line, &end, 16);
            if (end != line + 8 || *end != ' ')
                continue;
            /* word : asm : [canonical :] disassembly */
            char *c1 = strchr(line, ':');
            char *c2 = c1 == NULL ? NULL : strchr(c1 + 1, ':');
            char *c3 = c2 == NULL ? NULL : strchr(c2 + 1, ':');
            uint32_t alt = 0;
            if (c3 != NULL) {
                char *p = c2 + 1;
                while (*p == ' ')
                    p++;
                alt = (uint32_t)strtoul(p, NULL, 16);
            }
            instr_t *required = dec_copy((uint32_t)w, P_BASE);
            const char *expected = c3 != NULL ? c3 + 1 : c2 != NULL ? c2 + 1 : "";
            while (*expected == ' ')
                ++expected;
            bool expected_xx = strncmp(expected, "xx ", 3) == 0;
            bool got_xx = required != NULL && instr_get_opcode(required) == OP_xx;
            if (required == NULL || expected_xx != got_xx) {
                set_case((uint32_t)w, 1, files[f]);
                report(P_CANON, "source vector acceptance/rejection differs: %s", line);
            }
            if (required != NULL)
                instr_destroy(GD, required);
            if (g_nvec == cap) {
                cap *= 2;
                g_vec = realloc(g_vec, cap * sizeof(input_t));
            }
            g_vec[g_nvec++] = (input_t) { (uint32_t)w, alt, true };
        }
        fclose(fp);
    }
}

/***************************************************************************
 * Instructions built with the INSTR_CREATE_ macros.
 */
static reg_id_t
rx(rng_t *r)
{
    return DR_REG_X0 + (reg_id_t)rnd_n(r, 31);
}

static reg_id_t
rw(rng_t *r)
{
    return DR_REG_W0 + (reg_id_t)rnd_n(r, 31);
}

static reg_id_t
rx_or_sp(rng_t *r)
{
    return rnd_n(r, 8) == 0 ? DR_REG_XSP : rx(r);
}

#define R(x) opnd_create_reg(x)

/* A target near pc, 4-aligned, within range of a field of the given width. */
static app_pc
near_target(rng_t *r, int bits)
{
    int64_t range = 1LL << (bits - 1);
    int64_t off = (int64_t)rnd_n(r, 2 * range) - range;
    return (app_pc)(P_BASE + off * 4);
}

/* The macros evaluate their arguments more than once: every random choice is
 * made before the macro is used.
 */
static instr_t *
create_one(rng_t *r, int kind)
{
    int sz = rnd_n(r, 2) == 0 ? 8 : 4;
    opnd_size_t osz = sz == 8 ? OPSZ_8 : OPSZ_4;
    reg_id_t t = sz == 8 ? rx(r) : rw(r);
    reg_id_t t2 = sz == 8 ? rx(r) : rw(r);
    reg_id_t t3 = sz == 8 ? rx(r) : rw(r);
    reg_id_t x1 = rx(r), x2 = rx(r), w1 = rw(r);
    reg_id_t n = rx_or_sp(r);
    reg_id_t q1 = DR_REG_Q0 + (reg_id_t)rnd_n(r, 32);
    reg_id_t q2 = DR_REG_Q0 + (reg_id_t)rnd_n(r, 32);
    reg_id_t q3 = DR_REG_Q0 + (reg_id_t)rnd_n(r, 32);
    int imm12 = (int)rnd_n(r, 4096);
    int imm16 = (int)rnd_n(r, 65536);
    int hw = 16 * (int)rnd_n(r, sz / 2);
    int bit = (int)rnd_n(r, sz * 8);
    int pair_disp = ((int)rnd_n(r, 128) - 64) * sz;
    int simm9 = (int)rnd_n(r, 512) - 256;
    dr_pred_type_t cond = DR_PRED_EQ + (dr_pred_type_t)rnd_n(r, 14);
    app_pc b26 = near_target(r, 26), b19 = near_target(r, 19), b14 = near_target(r, 14);
    void *adr_tgt = (void *)(P_BASE + rnd_n(r, 1 << 20) - (1 << 19));
    void *adrp_tgt =
        (void *)((P_BASE + (rnd_n(r, 1ULL << 32) - (1ULL << 31))) & ~0xfffULL);
    void *lit_tgt = (void *)(P_BASE + 4 * (rnd_n(r, 1 << 18) - (1 << 17)));
    static const dr_extend_type_t ext[] = { DR_EXTEND_UXTW, DR_EXTEND_UXTX,
                                            DR_EXTEND_SXTW, DR_EXTEND_SXTX };
    int e = (int)rnd_n(r, 4);
    bool scaled = rnd_n(r, 2) == 0;
    int ext_amount = (int)rnd_n(r, 5);
    opnd_t elsz = rnd_n(r, 2) == 0 ? OPND_CREATE_BYTE() : OPND_CREATE_SINGLE();
    opnd_t mem0 = opnd_create_base_disp(n, DR_REG_NULL, 0, 0, osz);
    opnd_t memimm = opnd_create_base_disp(n, DR_REG_NULL, 0, imm12 * sz, osz);
    opnd_t mempair =
        opnd_create_base_disp(n, DR_REG_NULL, 0, pair_disp, sz == 8 ? OPSZ_16 : OPSZ_8);
    opnd_t memidx = opnd_create_base_disp_aarch64(n, (e == 0 || e == 2) ? w1 : x1, ext[e],
                                                  scaled, 0, 0, osz);
    /* Pre-index: the base must not be the destination. */
    reg_id_t pbase = DR_REG_X0 + (reg_id_t)rnd_n(r, 30);
    reg_id_t pdst = DR_REG_X0 + (pbase - DR_REG_X0 + 1 + rnd_n(r, 29)) % 30;
    opnd_t ob = opnd_create_reg(pbase), od = opnd_create_reg(pdst);
    opnd_t ot = R(t), ot2 = R(t2), ot3 = R(t3), ox1 = R(x1), ox2 = R(x2), ow1 = R(w1);
    opnd_t oq1 = R(q1), oq2 = R(q2), oq3 = R(q3);
    /* TBZ names a W register for bits 0-31. */
    opnd_t otb = R(bit < 32 ? reg_resize_to_opsz(t, OPSZ_4) : t);
    switch (kind) {
    case 0: return INSTR_CREATE_ldr(GD, ot, memimm);
    case 1: return INSTR_CREATE_str(GD, memimm, ot);
    case 2:
        if (t2 == t) {
            reg_id_t base = sz == 8 ? DR_REG_X0 : DR_REG_W0;
            ot2 = R(base + (t - base + 1) % 30);
        }
        return INSTR_CREATE_ldp(GD, ot, ot2, mempair);
    case 3: return INSTR_CREATE_stp(GD, mempair, ot, ot2);
    case 4: return INSTR_CREATE_ldr(GD, ot, memidx);
    case 5: return INSTR_CREATE_add(GD, ot, ot2, ot3);
    case 6: return INSTR_CREATE_sub(GD, ot, ot2, OPND_CREATE_INT(imm12));
    case 7:
        return INSTR_CREATE_add_shift(GD, ot, ot2, ot3, OPND_CREATE_INT8(DR_SHIFT_LSL),
                                      OPND_CREATE_INT8(bit));
    case 8:
        return INSTR_CREATE_add_extend(GD, ox1, ox2, ow1, OPND_CREATE_INT(DR_EXTEND_UXTW),
                                       OPND_CREATE_INT(ext_amount));
    case 9:
        return INSTR_CREATE_movz(GD, ot, OPND_CREATE_INT16(imm16), OPND_CREATE_INT8(hw));
    case 10:
        return INSTR_CREATE_movk(GD, ot, OPND_CREATE_INT16(imm16), OPND_CREATE_INT8(hw));
    case 11: return XINST_CREATE_move(GD, ot, ot2);
    case 12: return INSTR_CREATE_ldxr(GD, ot, mem0);
    case 13: return INSTR_CREATE_stxr(GD, mem0, ow1, ot);
    case 14: return INSTR_CREATE_ldaxr(GD, ot, mem0);
    case 15: return instr_create_2dst_3src(GD, OP_cas, ot, mem0, ot, ot2, mem0);
    case 16: return INSTR_CREATE_b(GD, opnd_create_pc(b26));
    case 17: return INSTR_CREATE_bl(GD, opnd_create_pc(b26));
    case 18: return INSTR_PRED(INSTR_CREATE_bcond(GD, opnd_create_pc(b19)), cond);
    case 19: return INSTR_CREATE_cbz(GD, opnd_create_pc(b19), ot);
    case 20:
        return INSTR_CREATE_tbnz(GD, opnd_create_pc(b14), otb, OPND_CREATE_INT8(bit));
    case 21: return INSTR_CREATE_adr(GD, ox1, opnd_create_rel_addr(adr_tgt, OPSZ_0));
    case 22: return INSTR_CREATE_adrp(GD, ox1, opnd_create_rel_addr(adrp_tgt, OPSZ_0));
    case 23: return INSTR_CREATE_ldr(GD, ot, opnd_create_rel_addr(lit_tgt, osz));
    case 24: return INSTR_CREATE_br(GD, ox1);
    case 25: return INSTR_CREATE_blr(GD, ox1);
    case 26: return INSTR_CREATE_ret(GD, ox1);
    case 27: return instr_create_2dst_3src(GD, OP_ldr, od, ob,
        opnd_create_base_disp_aarch64(opnd_get_reg(ob), DR_REG_NULL, DR_EXTEND_UXTX,
                                     false, simm9, DR_OPND_DEFAULT, OPSZ_PTR),
        ob, OPND_CREATE_INT(simm9));
    case 28: return INSTR_CREATE_add_vector(GD, oq1, oq2, oq3, elsz);
    case 29: return instr_create_2dst_2src(GD, OP_ldadd, ot2, mem0, ot, mem0);
    default: return INSTR_CREATE_nop(GD);
    }
}
#define NUM_CREATE 30

static void
run_created(uint64_t seed, int count)
{
    char why[256];
    for (int i = 0; i < count; i++) {
        rng_t r = rng_for(seed, 0xc0000000ULL + i);
        int kind = (int)rnd_n(&r, NUM_CREATE);
        instr_t *c = create_one(&r, kind);
        char d[128];
        dis_instr(c, d, sizeof(d));
        set_case(0, seed + i, "created");
        snprintf(g_dis, sizeof(g_dis), "%s", d);
        checked[P_CREATE]++;
        n_created++;
        uint32_t e;
        if (!enc_at(c, P_BASE, &e)) {
            report(P_CREATE, "INSTR_CREATE kind %d does not encode", kind);
            instr_destroy(GD, c);
            continue;
        }
        g_word = e;
        instr_t *back = dec_copy(e, P_BASE);
        if (back == NULL || !instr_strict(c, back, true, false, why, sizeof(why)) ||
            !instr_same(c, back)) {
            /* The one representational difference we know of. */
            bool shifted = false;
            for (int s = 0; back != NULL && s < instr_num_srcs(c); s++) {
                opnd_t o = instr_get_src(c, s);
                if (opnd_is_reg(o) &&
                    (opnd_get_flags(o) & (DR_OPND_SHIFTED | DR_OPND_EXTENDED)) != 0) {
                    o = opnd_create_reg_ex(opnd_get_reg(o), 0,
                                           opnd_get_flags(o) &
                                               ~(DR_OPND_SHIFTED | DR_OPND_EXTENDED));
                    instr_set_src(c, s, o);
                    shifted = true;
                }
            }
            if (shifted && instr_strict(c, back, true, false, why, sizeof(why)))
                known(P_CREATE, K_CREATE_SHIFTED, "kind %d", kind);
            else {
                report(P_CREATE,
                       "kind %d encodes as 0x%08x, which decodes differently: %s", kind,
                       e, back == NULL ? "invalid" : why);
            }
        }
        if (back != NULL)
            instr_destroy(GD, back);
        instr_destroy(GD, c);
        input_t inp = { e, 0, false };
        run_word(&inp, seed + i, "created");
    }
}

/***************************************************************************
 * Instruction lists with labels.
 */
typedef struct {
    int kind;    /* 0 body, 1 label, 2 branch, 3 pad */
    int label;   /* for branches: the label index */
    int bkind;   /* branch kind */
    instr_t *in; /* the instruction (for labels: the label) */
    int64_t off; /* byte offset in the encoding */
    int npad;    /* for pads */
} item_t;

#define MAX_ITEMS 48

/* Branch kinds and their offset field widths (in words; adr in bytes). */
static const struct {
    const char *name;
    int bits, scale;
} bkinds[] = {
    { "b", 26, 2 },    { "bl", 26, 2 },  { "b.cond", 19, 2 }, { "cbz", 19, 2 },
    { "cbnz", 19, 2 }, { "tbz", 14, 2 }, { "tbnz", 14, 2 },   { "adr", 21, 0 },
};
#define NUM_BKINDS (sizeof(bkinds) / sizeof(bkinds[0]))

static instr_t *
make_branch(rng_t *r, int bk, instr_t *label, reg_id_t *reg_out)
{
    opnd_t t = opnd_create_instr(label);
    reg_id_t reg = rnd_n(r, 2) == 0 ? rx(r) : rw(r);
    *reg_out = reg;
    switch (bk) {
    case 0: return INSTR_CREATE_b(GD, t);
    case 1: return INSTR_CREATE_bl(GD, t);
    case 2:
        return INSTR_PRED(INSTR_CREATE_bcond(GD, t),
                          DR_PRED_EQ + (dr_pred_type_t)rnd_n(r, 14));
    case 3: return INSTR_CREATE_cbz(GD, t, R(reg));
    case 4: return INSTR_CREATE_cbnz(GD, t, R(reg));
    case 5:
    case 6: {
        /* TBZ names a W register for bits 0-31. */
        int bit = (int)rnd_n(r, 64);
        reg =
            bit < 32 ? reg_resize_to_opsz(reg, OPSZ_4) : reg_resize_to_opsz(reg, OPSZ_8);
        *reg_out = reg;
        opnd_t ob = OPND_CREATE_INT8(bit), oreg = R(reg);
        if (bk == 5)
            return INSTR_CREATE_tbz(GD, t, oreg, ob);
        return INSTR_CREATE_tbnz(GD, t, oreg, ob);
    }
    default: return INSTR_CREATE_adr(GD, R(rx(r)), t);
    }
}

/* The pool of body instructions: vector words that are not PC-relative. */
static uint32_t *g_body;
static int g_nbody;

static void
build_body_pool(void)
{
    g_body = malloc(sizeof(uint32_t) * (g_nvec + 1));
    for (int i = 0; i < g_nvec; i++) {
        instr_t *in = dec_copy(g_vec[i].word, P_BASE);
        if (in == NULL)
            continue;
        pcrel_t pr;
        get_pcrel(in, &pr);
        uint32_t e;
        int opc = instr_get_opcode(in);
        if (!pr.pcrel && opc != OP_xx && !uses_sve(in) &&
            !sve_or_sme_space(g_vec[i].word) && enc_at(in, P_BASE, &e) &&
            e == g_vec[i].word)
            g_body[g_nbody++] = g_vec[i].word;
        instr_destroy(GD, in);
    }
}

static void
run_ilist(uint64_t seed, int idx, int far)
{
    char why[256];
    rng_t r = rng_for(seed, 0x11000000ULL + idx);
    item_t it[MAX_ITEMS];
    int n = 0;
    int nlabels = 1 + (int)rnd_n(&r, 3);
    int nbody = 2 + (int)rnd_n(&r, 12);
    int nbr = 1 + (int)rnd_n(&r, 5);
    instr_t *labels[4];
    for (int i = 0; i < nlabels; i++)
        labels[i] = INSTR_CREATE_label(GD);
    /* Random order of bodies, labels, branches and (maybe) one pad. */
    int total = nlabels + nbody + nbr + (far > 0 ? 1 : 0);
    int kinds[MAX_ITEMS];
    int k = 0;
    for (int i = 0; i < nlabels; i++)
        kinds[k++] = 1;
    for (int i = 0; i < nbody; i++)
        kinds[k++] = 0;
    for (int i = 0; i < nbr; i++)
        kinds[k++] = 2;
    if (far > 0)
        kinds[k++] = 3;
    for (int i = total - 1; i > 0; i--) {
        int j = (int)rnd_n(&r, i + 1);
        int t = kinds[i];
        kinds[i] = kinds[j];
        kinds[j] = t;
    }
    set_case(0, seed + idx, "ilist");
    snprintf(g_dis, sizeof(g_dis), "ilist %d (%d items%s)", idx, total,
             far > 0 ? ", far" : "");
    snprintf(g_ctx, sizeof(g_ctx), "ilist %d seed=0x%llx", idx,
             (unsigned long long)(seed + idx));
    instrlist_t *il = instrlist_create(GD);
    int li = 0, slot = SLOT_ILIST;
    int64_t off = 0;
    reg_id_t dummy;
    for (int i = 0; i < total; i++) {
        item_t *x = &it[n++];
        memset(x, 0, sizeof(*x));
        x->kind = kinds[i];
        x->off = off;
        switch (x->kind) {
        case 0: {
            uint32_t w = g_body[rnd_n(&r, g_nbody)];
            x->in = rnd_n(&r, 2) == 0 ? dec_inplace(w, slot++) : dec_copy(w, P_BASE);
            instrlist_append(il, x->in);
            off += 4;
            break;
        }
        case 1:
            x->in = labels[li++];
            instrlist_append(il, x->in);
            break;
        case 2:
            x->label = (int)rnd_n(&r, nlabels);
            x->bkind = (int)rnd_n(&r, NUM_BKINDS);
            if (far > 0 && rnd_n(&r, 2) == 0)
                x->bkind = far > 20000 ? 2 + (int)rnd_n(&r, 3) : 5 + (int)rnd_n(&r, 2);
            x->in = make_branch(&r, x->bkind, labels[x->label], &dummy);
            instrlist_append(il, x->in);
            off += 4;
            break;
        case 3:
            x->npad = far;
            for (int j = 0; j < far; j++)
                instrlist_append(il, INSTR_CREATE_nop(GD));
            off += 4LL * far;
            break;
        }
    }
    int64_t len = off;
    /* Label offsets, and whether every branch reaches its label. */
    int64_t loff[4] = { 0 };
    for (int i = 0; i < n; i++) {
        if (it[i].kind == 1) {
            for (int j = 0; j < nlabels; j++) {
                if (labels[j] == it[i].in)
                    loff[j] = it[i].off;
            }
        }
    }
    bool all_rep = true;
    for (int i = 0; i < n; i++) {
        if (it[i].kind != 2)
            continue;
        int64_t d = loff[it[i].label] - it[i].off;
        int bits = bkinds[it[i].bkind].bits;
        int64_t units = d >> bkinds[it[i].bkind].scale;
        if (units < -(1LL << (bits - 1)) || units >= (1LL << (bits - 1)))
            all_rep = false;
    }
    uint64_t final = P_BASE + 4 * rnd_n(&r, 1 << 20);
    byte *buf = malloc(len + 16);
    byte *end =
        instrlist_encode_to_copy(GD, il, buf, (byte *) final, buf + len + 16, true);
    checked[P_ILIST]++;
    n_ilists++;
    n_ilist_far += far > 0;
    if (!all_rep) {
        n_ilist_rejected += end == NULL;
        if (end != NULL)
            report(P_ILIST, "a branch cannot reach its label but the list encoded");
        goto done;
    }
    if (end != buf + len) {
        report(P_ILIST, "encoded %lld bytes, expected %lld",
               end == NULL ? -1LL : (long long)(end - buf), (long long)len);
        goto done;
    }
    for (int i = 0; i < n; i++) {
        if (it[i].kind != 0 && it[i].kind != 2)
            continue;
        uint32_t w;
        memcpy(&w, buf + it[i].off, 4);
        instr_t *at = dec_copy(w, final + it[i].off);
        if (at == NULL) {
            report(P_ILIST, "item %d: 0x%08x does not decode", i, w);
            goto done;
        }
        if (it[i].kind == 0) {
            if (!instr_strict(it[i].in, at, false, false, why, sizeof(why))) {
                report(P_ILIST, "item %d: body instruction 0x%08x differs: %s", i, w,
                       why);
                instr_destroy(GD, at);
                goto done;
            }
        } else {
            uint64_t want = final + loff[it[i].label];
            uint64_t got = pcrel_target_of(instr_get_src(at, 0));
            /* The other operands are the same; source 0 is the target. */
            if (got != want) {
                report(P_ILIST, "item %d: %s 0x%08x targets 0x%llx, label is at 0x%llx",
                       i, bkinds[it[i].bkind].name, w, (unsigned long long)got,
                       (unsigned long long)want);
                instr_destroy(GD, at);
                goto done;
            }
            if (!instr_strict(it[i].in, at, true, true, why, sizeof(why))) {
                report(P_ILIST, "item %d: %s 0x%08x differs: %s", i,
                       bkinds[it[i].bkind].name, w, why);
                instr_destroy(GD, at);
                goto done;
            }
        }
        instr_destroy(GD, at);
    }
done:
    free(buf);
    instrlist_clear_and_destroy(GD, il);
}

/***************************************************************************
 * Driver.
 */
static void
usage(void)
{
    fprintf(stderr,
            "usage: a64_ir_transform [-seed S] [-n N] [-created N] [-ilists N] "
            "[-props a,b] [-word W] [-max-print M] [-strict] [-no-floors] [-v] "
            "<dis-a64*.txt...>\n");
    exit(2);
}

int
main(int argc, char **argv)
{
#ifdef DYNAMORIO_STANDALONE
    replay_context = dr_standalone_init();
    if (replay_context == NULL)
        return 2;
    atexit(dr_standalone_exit);
#endif
    uint64_t seed = 1;
    int nmut = 400000, ncreated = 40000, nilists = 8000;
    bool floors = true;
    long long one_word = -1;
    const char *props = NULL;
    int nfiles = 0;
    char **files = calloc(argc, sizeof(char *));
    if (getenv("A64_IRT_SEED") != NULL)
        seed = strtoull(getenv("A64_IRT_SEED"), NULL, 0);
    if (getenv("A64_IRT_STRICT") != NULL && atoi(getenv("A64_IRT_STRICT")) != 0)
        opt_strict = true;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "-seed") == 0 && i + 1 < argc)
            seed = strtoull(argv[++i], NULL, 0);
        else if (strcmp(argv[i], "-n") == 0 && i + 1 < argc)
            nmut = atoi(argv[++i]);
        else if (strcmp(argv[i], "-created") == 0 && i + 1 < argc)
            ncreated = atoi(argv[++i]);
        else if (strcmp(argv[i], "-ilists") == 0 && i + 1 < argc)
            nilists = atoi(argv[++i]);
        else if (strcmp(argv[i], "-props") == 0 && i + 1 < argc)
            props = argv[++i];
        else if (strcmp(argv[i], "-word") == 0 && i + 1 < argc)
            one_word = strtoll(argv[++i], NULL, 0);
        else if (strcmp(argv[i], "-max-print") == 0 && i + 1 < argc)
            opt_max_print = atoi(argv[++i]);
        else if (strcmp(argv[i], "-strict") == 0)
            opt_strict = true;
        else if (strcmp(argv[i], "-no-floors") == 0)
            floors = false;
        else if (strcmp(argv[i], "-v") == 0)
            opt_verbose = true;
        else if (argv[i][0] == '-')
            usage();
        else
            files[nfiles++] = argv[i];
    }
    for (int p = 0; p < NUM_PROPS; p++)
        prop_on[p] = props == NULL;
    if (props != NULL) {
        char *s = strdup(props);
        for (char *t = strtok(s, ","); t != NULL; t = strtok(NULL, ",")) {
            bool found = false;
            for (int p = 0; p < NUM_PROPS; p++) {
                if (strcmp(t, prop_names[p]) == 0)
                    prop_on[p] = found = true;
            }
            if (!found)
                usage();
        }
        free(s);
        floors = false;
    }
    signal(SIGSEGV, crash_handler);
    signal(SIGBUS, crash_handler);
    signal(SIGABRT, crash_handler);
    signal(SIGILL, crash_handler);
    signal(SIGTRAP, crash_handler);
    for (int i = 0; i < 1024; i++)
        virt_to_fam[i] = -1;

    g_slots = mmap(NULL, NUM_SLOTS * 4, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (g_slots == MAP_FAILED) {
        perror("mmap");
        return 2;
    }
    disassemble_set_syntax(DR_DISASM_DR);
    instr_t *pre = dec_copy(0xf8400c20, P_BASE);
    instr_t *post = dec_copy(0xf8400420, P_BASE);
    if (pre == NULL || post == NULL ||
        !public_pre_index(instr_get_src(pre, 0)) ||
        public_pre_index(instr_get_src(post, 0)) ||
        instr_strict(pre, post, false, false, NULL, 0) ||
        !only_pre_index_differs(pre, post)) {
        fprintf(stderr, "public pre/post-index negative control failed\n");
        return 1;
    }
    instr_destroy(GD, pre);
    instr_destroy(GD, post);

    if (one_word >= 0) {
        floors = false;
        input_t inp = { (uint32_t)one_word, 0, false };
        if (!run_word(&inp, seed, "word"))
            printf("word 0x%08x is not usable (invalid, OP_xx or SVE)\n",
                   (uint32_t)one_word);
    } else {
        if (nfiles == 0)
            usage();
        load_vectors(nfiles, files);
        /* Learn the regdeps family map first, so every check can use it. */
        if (prop_on[P_REGDEPS]) {
            for (int pass = 0; pass < 2; pass++) {
                for (int i = 0; i < g_nvec; i++) {
                    instr_t *in = dec_copy(g_vec[i].word, P_BASE);
                    if (in == NULL)
                        continue;
                    if (instr_get_opcode(in) != OP_xx && !uses_sve(in)) {
                        usage_t u;
                        famset_t m, v;
                        char why[256];
                        get_usage(in, &u);
                        model_regdeps(in, &u, &m);
                        instr_t *rd = instr_create(GD);
                        instr_convert_to_isa_regdeps(GD, in, rd);
                        regdeps_sets(rd, &v);
                        if (m.nsrc == v.nsrc && m.ndst == v.ndst) {
                            /* Conflicts are reported by the checks. */
                            learn_side(m.src, m.nsrc, v.src, v.nsrc, why, sizeof(why));
                            learn_side(m.dst, m.ndst, v.dst, v.ndst, why, sizeof(why));
                        }
                        instr_destroy(GD, rd);
                    }
                    instr_destroy(GD, in);
                }
            }
        }
        for (int i = 0; i < g_nvec; i++) {
            if (run_word(&g_vec[i], seed + i, "vector"))
                n_vectors++;
            else
                n_vector_skipped++;
        }
        for (int i = 0; i < nmut; i++) {
            rng_t r = rng_for(seed, 0x80000000ULL + i);
            input_t inp = g_vec[rnd_n(&r, g_nvec)];
            uint32_t orig = inp.word;
            int flips = 1 + (int)rnd_n(&r, 3);
            for (int f = 0; f < flips; f++)
                inp.word ^= 1U << rnd_n(&r, 24);
            inp.alt = 0;
            inp.vector = false;
            if (run_word(&inp, seed + i, "mutant")) {
                n_mutants++;
                if (prop_on[P_CANON])
                    prop_same(inp.word, orig);
            } else
                n_mutant_rejected++;
        }
        run_created(seed, ncreated);
        if (prop_on[P_ILIST]) {
            build_body_pool();
            for (int i = 0; i < nilists; i++) {
                /* A few lists have a pad that a short branch cannot cross. */
                int far = 0;
                if (i % 64 == 63)
                    far = 8200;
                else if (i == nilists / 2)
                    far = 270000;
                run_ilist(seed, i, far);
            }
        }
    }

    printf("\nvectors %llu (skipped %llu: SVE, OP_xx or invalid), mutants %llu "
           "(rejected %llu), created %llu, lists %llu (%llu with far pads, %llu "
           "rejected as unreachable)\n",
           (unsigned long long)n_vectors, (unsigned long long)n_vector_skipped,
           (unsigned long long)n_mutants, (unsigned long long)n_mutant_rejected,
           (unsigned long long)n_created, (unsigned long long)n_ilists,
           (unsigned long long)n_ilist_far, (unsigned long long)n_ilist_rejected);
    printf("instr_same() pairs %llu (%llu pre/post-index #0); clone in place %llu; "
           "rename api %llu/%llu/%llu, in place %llu, skipped "
           "%llu; reloc PC-relative %llu, out of range rejected %llu; regdeps "
           "families learned %llu, unmapped %llu\n",
           (unsigned long long)n_same_checked, (unsigned long long)n_same_prepost,
           (unsigned long long)n_clone_inplace, (unsigned long long)n_rename_api[0],
           (unsigned long long)n_rename_api[1], (unsigned long long)n_rename_api[2],
           (unsigned long long)n_rename_inplace, (unsigned long long)n_rename_skipped,
           (unsigned long long)n_reloc_pcrel, (unsigned long long)n_reloc_rejected,
           (unsigned long long)n_regdeps_learned, (unsigned long long)n_regdeps_unmapped);
    printf("%-8s %10s %8s %8s %10s\n", "property", "checked", "failed", "known", "floor");
    bool ok = true;
    for (int p = 0; p < NUM_PROPS; p++) {
        if (!prop_on[p])
            continue;
        bool short_ = floors && checked[p] < prop_floor[p];
        printf("%-8s %10llu %8llu %8llu %10llu%s\n", prop_names[p],
               (unsigned long long)checked[p], (unsigned long long)failed[p],
               (unsigned long long)known_cnt[p], (unsigned long long)prop_floor[p],
               short_ ? "  QUOTA NOT MET" : "");
        if (failed[p] > 0 || short_)
            ok = false;
    }
    for (int k = 0; k < NUM_KNOWN; k++) {
        if (known_hits[k] > 0) {
            printf("known %s (since %s): %llu: %s\n", known_info[k].name,
                   known_info[k].since, (unsigned long long)known_hits[k],
                   known_info[k].reason);
        }
    }
    /* Floors for the coverage that matters most. */
    if (floors &&
        (n_reloc_pcrel < 200000 || n_reloc_rejected < 60000 || n_ilist_rejected < 30 ||
         n_rename_inplace < 120000)) {
        printf("QUOTA NOT MET: PC-relative relocations %llu (200000), rejections %llu "
               "(60000), unreachable lists %llu (30), in-place renames %llu (120000)\n",
               (unsigned long long)n_reloc_pcrel, (unsigned long long)n_reloc_rejected,
               (unsigned long long)n_ilist_rejected,
               (unsigned long long)n_rename_inplace);
        ok = false;
    }
    printf("%s\n", ok ? "all properties hold" : "FAILED");
    return ok ? 0 : 1;
}
