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

/* Checks that the A64 encoder never silently changes what it was asked to
 * encode.  The encoder's per-field helpers can mask a value down to the width
 * of its field (an out-of-range immediate, a W register where an X is
 * required, an unsupported element size) and still pass the generated
 * encoder's field-mask cross-check, producing a different instruction.
 *
 * Each input decodes a real instruction as a template, applies a list of
 * operand mutations, and encodes the result at a fuzzed pc.  If the encoder
 * accepts it, the bits are decoded again and every operand must match what
 * was requested.  Encoding failure is acceptable for mutated operands, which
 * can make the request unencodable, but not for the valid population:
 *  - a template with no mutation records must encode;
 *  - with flags bit 1 (and not bit 0), the template's general-purpose
 *    registers are renamed by an even rotation of x0-x29 (which keeps equal
 *    registers equal and register pairs even and consecutive), and the result
 *    must encode.
 * This finite saved-reproducer adapter reports attempts and successes per
 * population. The original broad generated-corpus minimum is not a saved-input
 * floor; that separate population remains pending. No assertion hooks are used.
 *
 * Input layout:
 *   byte 0      SVE vector length index into replay_sve_vls[]
 *   byte 1      flags: bit 0 builds the instruction from scratch (opcode and
 *               operand counts from the template word, operands from the
 *               mutation records) instead of decoding the template; bit 1
 *               (without bit 0) selects the valid population: the first
 *               mutation record's byte 0 picks the register rotation and no
 *               mutations are applied
 *   bytes 2-9   encode pc
 *   bytes 10-13 template A64 word
 *   bytes 14-   mutation records of MUTATION_SIZE bytes:
 *                 byte 0  target: bit 7 selects dst (1) or src (0), rest index
 *                 byte 1  mutation kind
 *                 bytes 2-9 payload
 */

#include "input.h"
#include "source_selectors.h"

#include <unistd.h>

#define HEADER_SIZE 14
#define MUTATION_SIZE 10
#define MAX_MUTATIONS 8

static const opnd_size_t sizes[] = {
    OPSZ_NA, OPSZ_0,  OPSZ_1,  OPSZ_2,   OPSZ_4,   OPSZ_8,   OPSZ_16,
    OPSZ_32, OPSZ_64, OPSZ_1b, OPSZ_2b,  OPSZ_3b,  OPSZ_4b,  OPSZ_5b,
    OPSZ_6b, OPSZ_7b, OPSZ_9b, OPSZ_10b, OPSZ_12b, OPSZ_20b, OPSZ_25b,
    OPSZ_6,  OPSZ_12, OPSZ_24, OPSZ_48,  OPSZ_128, OPSZ_256, OPSZ_512,
};
#define NUM_SIZES (sizeof(sizes) / sizeof(sizes[0]))

static const opnd_size_t element_sizes[] = { OPSZ_1, OPSZ_2, OPSZ_4, OPSZ_8, OPSZ_16 };
#define NUM_ELEMENT_SIZES (sizeof(element_sizes) / sizeof(element_sizes[0]))

/* Values near field boundaries are where masking bugs hide. */
static int64
interesting_value(uint64 payload, int64 old)
{
    /* Unsigned arithmetic throughout: wrapping is intended, not UB. */
    uint k = (uint)((payload >> 3) % 64);
    uint64 small = (uint64)(int64)(int8_t)(payload >> 8);
    switch (payload & 7) {
    case 0: return (int64)(payload >> 3);
    case 1: return (int64)(1ULL << k);
    case 2: return (int64)((1ULL << k) - 1);
    case 3: return (int64)(0 - (1ULL << k));
    case 4: return (int64)(0 - (1ULL << k) - 1);
    case 5: return (int64)small;
    case 6: return (int64)((uint64)old + small);
    default: return (int64)((uint64)old << (k % 16));
    }
}

static reg_id_t
pick_reg(uint64 payload)
{
    /* Mostly stay within the architectural register banks, sometimes go
     * anywhere in the enum.
     */
    if ((payload & 3) == 0)
        return source_reg((payload >> 2) % SOURCE_REG_COUNT);
    static const reg_id_t banks[] = { DR_REG_X0, DR_REG_W0, DR_REG_Q0,
                                      DR_REG_D0, DR_REG_S0, DR_REG_H0,
                                      DR_REG_B0, DR_REG_Z0, DR_REG_P0 };
    reg_id_t base = banks[(payload >> 2) % (sizeof(banks) / sizeof(banks[0]))];
    uint limit = base == DR_REG_P0                 ? 16
        : (base == DR_REG_X0 || base == DR_REG_W0) ? 33
                                                   : 32;
    return (reg_id_t)(base + (payload >> 8) % limit);
}

static opnd_t
rebuild_mem(opnd_t old, reg_id_t base, reg_id_t index, int disp, opnd_size_t size)
{
    bool scaled = false;
    uint amount = 0;
    dr_extend_type_t ext = opnd_get_index_extend(old, &scaled, &amount);
    dr_opnd_flags_t flags = opnd_get_flags(old);
    if (opnd_is_vector_base_disp(old)) {
        return opnd_create_vector_base_disp_aarch64(
            base, index, opnd_get_vector_element_size(old), ext, scaled, disp, flags,
            size, amount);
    }
    opnd_t mem =
        opnd_create_base_disp_aarch64(base, index, ext, scaled, disp, flags, size);
    if (index != DR_REG_NULL)
        opnd_set_index_extend_value(&mem, ext, scaled, amount);
    return mem;
}

static opnd_t
mutate(opnd_t old, uint8_t kind, uint64 payload, byte *pc)
{
    switch (kind % 14) {
    case 0: /* immediate value */
        if (opnd_is_immed_int(old)) {
            return opnd_create_immed_int(
                (ptr_int_t)interesting_value(payload, opnd_get_immed_int(old)),
                opnd_get_size(old));
        }
        return opnd_create_immed_int((ptr_int_t)interesting_value(payload, 0),
                                     sizes[(payload >> 32) % NUM_SIZES]);
    case 1: /* operand size */
        opnd_set_size(&old, sizes[payload % NUM_SIZES]);
        return old;
    case 2: /* any register */ return opnd_create_reg(pick_reg(payload));
    case 3: /* nearby register, keeping element size and predicate flags */
        if (opnd_is_reg(old)) {
            int r = (int)source_reg_id(opnd_get_reg(old)) + (int8_t)payload;
            if (r <= 1 || r >= (int)SOURCE_REG_COUNT)
                return old;
            r = source_reg((unsigned)r);
            if (opnd_is_predicate_reg(old) && r >= DR_REG_P0 && r <= DR_REG_P15)
                return opnd_create_predicate_reg((reg_id_t)r,
                                                 opnd_is_predicate_merge(old));
            if (opnd_is_element_vector_reg(old)) {
                return opnd_create_reg_element_vector((reg_id_t)r,
                                                      opnd_get_vector_element_size(old));
            }
            return opnd_create_reg((reg_id_t)r);
        }
        return old;
    case 4: /* element size */
        if (opnd_is_reg(old)) {
            reg_id_t r = opnd_get_reg(old);
            if (r == DR_REG_NULL || r == DR_REG_INVALID)
                return old;
            if ((payload & 7) == 0)
                return opnd_create_reg(r);
            return opnd_create_reg_element_vector(
                r, element_sizes[(payload >> 3) % NUM_ELEMENT_SIZES]);
        }
        return old;
    case 5: /* memory displacement */
        if (opnd_is_base_disp(old)) {
            return rebuild_mem(old, opnd_get_base(old), opnd_get_index(old),
                               (int)interesting_value(payload, opnd_get_disp(old)),
                               opnd_get_size(old));
        }
        return old;
    case 6: /* memory base or index register */
        if (opnd_is_base_disp(old)) {
            reg_id_t r = pick_reg(payload >> 1);
            if ((payload & 1) == 0) {
                return rebuild_mem(old, r, opnd_get_index(old), opnd_get_disp(old),
                                   opnd_get_size(old));
            }
            return rebuild_mem(old, opnd_get_base(old), r, opnd_get_disp(old),
                               opnd_get_size(old));
        }
        return old;
    case 7: /* index extend and shift */
        if (opnd_is_base_disp(old) && opnd_get_index(old) != DR_REG_NULL) {
            opnd_set_index_extend_value(&old, (dr_extend_type_t)(payload % 8),
                                        (payload & 8) != 0, (uint)((payload >> 4) % 8));
        }
        return old;
    case 8: /* pc-relative target */
        return opnd_create_pc(
            (app_pc)((ptr_uint_t)pc + (ptr_uint_t)interesting_value(payload, 0)));
    case 9: /* float immediate */
        if ((payload & 1) == 0) {
            float f;
            uint bits = (uint)(payload >> 1);
            memcpy(&f, &bits, sizeof(f));
            return opnd_create_immed_float(f);
        } else {
            double d;
            uint64 bits = payload >> 1;
            memcpy(&d, &bits, sizeof(d));
            return opnd_create_immed_double(d);
        }
    case 10: /* predicate merge/zero */
        if (opnd_is_reg(old)) {
            reg_id_t r = opnd_get_reg(old);
            if (r >= DR_REG_P0 && r <= DR_REG_P15)
                return opnd_create_predicate_reg(r, (payload & 1) != 0);
        }
        return old;
    case 11: /* operand flags (shift/extend/negate markers) */
        opnd_set_flags(&old, (dr_opnd_flags_t)(payload & 0x7fff));
        return old;
    case 12: /* fresh scalar memory operand */
        return opnd_create_base_disp_aarch64(
            pick_reg(payload), (payload & 0x100) ? pick_reg(payload >> 9) : DR_REG_NULL,
            DR_EXTEND_UXTX, false, (int)interesting_value(payload >> 20, 0), 0,
            sizes[(payload >> 40) % NUM_SIZES]);
    default: /* fresh immediate with an unrelated size */
        return opnd_create_immed_int((ptr_int_t)interesting_value(payload, 0),
                                     sizes[(payload >> 40) % NUM_SIZES]);
    }
}

/* An operand of any kind the IR can represent, for scratch-built instructions:
 * operand kinds the decoder never pairs with an opcode reach encoder paths
 * that template mutation does not.
 */
static opnd_t
fresh_operand(uint8_t kind, uint64 payload, byte *pc)
{
    switch (kind % 8) {
    case 0: return opnd_create_reg(pick_reg(payload));
    case 1: {
        reg_id_t r = pick_reg(payload);
        if (r == DR_REG_NULL || r == DR_REG_INVALID)
            r = DR_REG_Z0;
        return opnd_create_reg_element_vector(
            r, element_sizes[(payload >> 40) % NUM_ELEMENT_SIZES]);
    }
    case 2:
        return opnd_create_predicate_reg(DR_REG_P0 + (payload >> 8) % 16,
                                         (payload & 1) != 0);
    case 3:
        return opnd_create_immed_int((ptr_int_t)interesting_value(payload, 0),
                                     sizes[(payload >> 40) % NUM_SIZES]);
    case 4: return mutate(opnd_create_null(), 9, payload, pc);  /* float */
    case 5: return mutate(opnd_create_null(), 12, payload, pc); /* scalar memory */
    case 6:
        return opnd_create_vector_base_disp_aarch64(
            (payload & 1) ? pick_reg(payload >> 1) : DR_REG_X0 + (payload >> 8) % 31,
            DR_REG_Z0 + (payload >> 16) % 32,
            element_sizes[(payload >> 24) % NUM_ELEMENT_SIZES],
            (dr_extend_type_t)((payload >> 28) % 8), (payload >> 31) & 1,
            (int)interesting_value(payload >> 32, 0), 0,
            sizes[(payload >> 44) % NUM_SIZES], (uint)((payload >> 52) % 4));
    default: return mutate(opnd_create_null(), 8, payload, pc); /* pc target */
    }
}

static bool
immeds_match(opnd_t want, opnd_t got)
{
    int64 w = opnd_get_immed_int(want), g = opnd_get_immed_int(got);
    if (w == g)
        return true;
    /* The decoder may present the same field bits signed or unsigned. */
    uint bits = opnd_size_in_bits(opnd_get_size(got));
    if (bits == 0 || bits >= 64)
        return false;
    uint64 mask = (1ULL << bits) - 1;
    return ((uint64)w & mask) == ((uint64)g & mask) &&
        /* ...but only if the requested value actually fits the field. */
        (w >= -(int64)(1ULL << (bits - 1)) && w <= (int64)mask);
}

/* Returns NULL if the operands match, else a description of the difference. */
static const char *
opnd_mismatch(int opcode, opnd_t want, opnd_t got)
{
    static char why[128];
    if (opnd_is_immed_int(want) && opnd_is_immed_int(got)) {
        if (immeds_match(want, got))
            return NULL;
        snprintf(why, sizeof(why), "immediate value 0x%llx (%u bits) became 0x%llx",
                 (unsigned long long)opnd_get_immed_int(want),
                 opnd_size_in_bits(opnd_get_size(want)),
                 (unsigned long long)opnd_get_immed_int(got));
        return why;
    }
    if (opnd_is_immed_float(want) && opnd_is_immed_float(got)) {
        float a = opnd_get_immed_float(want), b = opnd_get_immed_float(got);
        return memcmp(&a, &b, sizeof(a)) == 0 ? NULL : "float immediate";
    }
    if (opnd_is_reg(want) && opnd_is_reg(got)) {
        /* TBZ/TBNZ on a bit below 32 may name the X or the W register: the
         * encodings are identical and DR decodes the W form.
         */
        if (opnd_get_reg(want) != opnd_get_reg(got) &&
            !(reg_to_pointer_sized(opnd_get_reg(want)) ==
                  reg_to_pointer_sized(opnd_get_reg(got)) &&
              (opcode == OP_tbz || opcode == OP_tbnz)))
            return "register";
        if (opnd_is_element_vector_reg(want) && opnd_is_element_vector_reg(got) &&
            opnd_get_vector_element_size(want) != opnd_get_vector_element_size(got))
            return "vector element size";
        /* A mutated operand can carry both the merge and zero flags; only an
         * unambiguous request has a defined expected mode.
         */
        /* The decoder shows a predicate without /m or /z where the instruction
         * has no mode (e.g. reductions); there is nothing to compare then.
         */
        if (opnd_is_predicate_reg(want) && opnd_is_predicate_reg(got) &&
            (opnd_is_predicate_merge(got) || opnd_is_predicate_zero(got)) &&
            opnd_is_predicate_merge(want) != opnd_is_predicate_zero(want) &&
            opnd_is_predicate_merge(want) != opnd_is_predicate_merge(got))
            return "predicate merge/zero";
        return NULL;
    }
    if (opnd_is_base_disp(want) && opnd_is_base_disp(got)) {
        if (opnd_get_base(want) != opnd_get_base(got))
            return "memory base";
        if (opnd_get_index(want) != opnd_get_index(got))
            return "memory index";
        if (opnd_get_disp(want) != opnd_get_disp(got))
            return "memory displacement";
        /* Address-only operands (e.g. SVE ADR) decode with no size. */
        if (opnd_get_size(got) != OPSZ_0 && opnd_get_size(want) != opnd_get_size(got)) {
            snprintf(why, sizeof(why), "memory size %u became %u bytes",
                     opnd_size_in_bytes(opnd_get_size(want)),
                     opnd_size_in_bytes(opnd_get_size(got)));
            return why;
        }
        if (opnd_get_index(want) != DR_REG_NULL) {
            bool ws = false, gs = false;
            uint wa = 0, ga = 0;
            dr_extend_type_t we = opnd_get_index_extend(want, &ws, &wa);
            dr_extend_type_t ge = opnd_get_index_extend(got, &gs, &ga);
            /* A scaled index shifted by 0 is the same as an unscaled one. */
            if (we != ge || (ws && wa != 0) != (gs && ga != 0) || (ws && gs && wa != ga))
                return "index extend";
        }
        return NULL;
    }
    if (opnd_is_pc(want) && opnd_is_pc(got))
        return opnd_get_pc(want) == opnd_get_pc(got) ? NULL : "pc target";
    if (opnd_is_rel_addr(want) && opnd_is_rel_addr(got)) {
        if (opnd_get_addr(want) != opnd_get_addr(got))
            return "pc-relative address";
        /* ADR/ADRP operands decode with no size; literal loads have one. */
        if (opnd_get_size(got) != OPSZ_0 && opnd_get_size(want) != opnd_get_size(got))
            return "memory size";
        return NULL;
    }
    return opnd_same(want, got) ? NULL : "operand kind";
}

/* Gives instr new, empty operand lists of the given sizes.  Calling
 * instr_set_num_opnds() on an instr that already has operands is an API
 * misuse (a CLIENT_ASSERT this harness ignores) that leaks the old lists and
 * leaves a zero-count list unchanged, so start from a reset instr, keeping
 * the opcode and predicate (the condition of B.cond, CSEL, ...).
 */
static void
reset_operands(instr_t *instr, int ndst, int nsrc)
{
    int opc = instr_get_opcode(instr);
    dr_pred_type_t pred = instr_get_predicate(instr);
    instr_reset(GD, instr);
    instr_set_opcode(instr, opc);
    instr_set_predicate(instr, pred);
    instr_set_num_opnds(GD, instr, ndst, nsrc);
}

/* Populations, for the counts. */
enum { POP_MUTATED, POP_SCRATCH, POP_UNMUTATED, POP_RENAMED, POP_NUM };
static const char *const pop_names[POP_NUM] = { "mutated", "scratch", "unmutated",
                                                "renamed" };
static uint64 pop_tried[POP_NUM], pop_encoded[POP_NUM];
/* Renamed requests whose encoding differs from the template's, and SYS/SYSL
 * requests accepted as a named alias.
 */
static uint64 renamed_changed, sys_aliases;

static void
report_counts(void)
{
    for (int i = 0; i < POP_NUM; ++i)
        fprintf(stderr, "saved-encode: %s tried=%llu encoded=%llu\n",
                pop_names[i], (unsigned long long)pop_tried[i],
                (unsigned long long)pop_encoded[i]);
    fprintf(stderr, "saved-encode: changed=%llu aliases=%llu\n",
            (unsigned long long)renamed_changed, (unsigned long long)sys_aliases);
}

/* Renames x0-x29/w0-w29 by rot (even), keeping x30, sp and zr. */
static reg_id_t
rename_gpr(reg_id_t r, int rot)
{
    if (r >= DR_REG_X0 && r <= DR_REG_X29)
        return DR_REG_X0 + (r - DR_REG_X0 + rot) % 30;
    if (r >= DR_REG_W0 && r <= DR_REG_W29)
        return DR_REG_W0 + (r - DR_REG_W0 + rot) % 30;
    return r;
}

static opnd_t
rename_opnd(opnd_t op, int rot)
{
    if (opnd_is_reg(op)) {
        reg_id_t r = opnd_get_reg(op), n = rename_gpr(r, rot);
        if (n == r)
            return op;
        /* Keep any flags (e.g. shift/extend markers) the operand carries. */
        opnd_replace_reg(&op, r, n);
        return op;
    }
    if (opnd_is_base_disp(op)) {
        reg_id_t base = opnd_get_base(op), index = opnd_get_index(op);
        if (base != DR_REG_NULL)
            opnd_replace_reg(&op, base, rename_gpr(base, rot));
        if (index != DR_REG_NULL && index != base)
            opnd_replace_reg(&op, index, rename_gpr(index, rot));
        return op;
    }
    return op;
}

/* The general-purpose register number of r, or -1. */
static int
gpr_num(reg_id_t r)
{
    if (r >= DR_REG_X0 && r <= DR_REG_X30)
        return r - DR_REG_X0;
    if (r >= DR_REG_W0 && r <= DR_REG_W30)
        return r - DR_REG_W0;
    return -1;
}

/* Whether register number n is in one of the A64 register fields (Rt/Rd,
 * Rn, Rt2/Ra, Rm/Rs) of word, or is the second of a pair whose first is.
 */
static bool
gpr_in_fields(uint word, int n)
{
    static const int shift[] = { 0, 5, 10, 16 };
    for (int i = 0; i < 4; i++) {
        int f = (word >> shift[i]) & 31;
        if (f == n || (f % 2 == 0 && f + 1 == n))
            return true;
    }
    return false;
}

/* Whether every renameable register operand of instr is named by the
 * encoding: implicit operands (e.g. x16/x17 of AUTIA1716) cannot be renamed.
 */
static bool
gprs_all_encoded(instr_t *instr, uint word)
{
    for (int pass = 0; pass < 2; pass++) {
        int num = pass == 0 ? instr_num_dsts(instr) : instr_num_srcs(instr);
        for (int i = 0; i < num; i++) {
            opnd_t op = pass == 0 ? instr_get_dst(instr, i) : instr_get_src(instr, i);
            for (int k = 0; k < opnd_num_regs_used(op); k++) {
                int n = gpr_num(opnd_get_reg_used(op, k));
                if (n >= 0 && n < 30 && !gpr_in_fields(word, n))
                    return false;
            }
        }
    }
    return true;
}

/* Whether the valid population leaves out this opcode: renaming registers
 * keeps an instruction valid except where an encoding constrains which
 * registers it may name beyond equality and pairing.
 */
static bool
rename_exempt(int opc)
{
    /* LS64: eight consecutive registers from an even Xt below x24 (a
     * rotation can wrap the group past x29).
     */
    return strcmp(decode_opcode_name(opc), "ld64b") == 0 ||
        strcmp(decode_opcode_name(opc), "st64b") == 0 ||
        strcmp(decode_opcode_name(opc), "st64bv") == 0 ||
        strcmp(decode_opcode_name(opc), "st64bv0") == 0;
}

/* SYS/SYSL operands can spell an operation DR decodes by name (DC, IC, AT,
 * TLBI...).  That is only the same instruction if the encoding holds exactly
 * the requested op1, CRn, CRm, op2 and register.
 */
static bool
sys_alias_matches(instr_t *want, uint enc)
{
    if (instr_num_dsts(want) != 1 || instr_num_srcs(want) != 4)
        return false;
    static const int shift[4] = { 16, 12, 8, 5 }, width[4] = { 3, 4, 4, 3 };
    for (int i = 0; i < 4; i++) {
        opnd_t imm = instr_get_src(want, i);
        if (!opnd_is_immed_int(imm))
            return false;
        ptr_int_t v = opnd_get_immed_int(imm);
        if (v < 0 || v >= (1 << width[i]) ||
            (uint)v != ((enc >> shift[i]) & ((1u << width[i]) - 1)))
            return false;
    }
    opnd_t rt = instr_get_dst(want, 0);
    if (!opnd_is_reg(rt))
        return false;
    reg_id_t r = opnd_get_reg(rt);
    uint num;
    if (r >= DR_REG_X0 && r <= DR_REG_X30)
        num = r - DR_REG_X0;
    else if (r == DR_REG_XZR)
        num = 31;
    else
        return false;
    return (enc & 31) == num;
}

static void
check_roundtrip(instr_t *want, byte *pc, uint template_word, int pop)
{
    char buf1[256], buf2[256];
    uint enc = 0;
    instr_set_raw_bits_valid(want, false);
    pop_tried[pop]++;
    byte *end = instr_encode_to_copy(GD, want, (byte *)&enc, pc);
    if (end == NULL) {
        if (pop == POP_UNMUTATED || pop == POP_RENAMED) {
            instr_disassemble_to_buffer(GD, want, buf1, sizeof(buf1));
            REPLAY_CHECK(false, "template %08x: valid %s request failed to encode: %s",
                       template_word, pop_names[pop], buf1);
        }
        return;
    }
    pop_encoded[pop]++;
    if (pop == POP_RENAMED && enc != template_word)
        renamed_changed++;
    REPLAY_CHECK(end == (byte *)&enc + 4, "encode returned length %td", end - (byte *)&enc);

    instr_t got;
    instr_init(GD, &got);
    decode_from_copy(GD, (byte *)&enc, pc, &got);
    /* Equivalent representations, not encoder bugs:
     * - SYS/SYSL operands can spell an operation DR decodes by name (DC, IC...);
     * - a logical immediate's raw-field operand is only kept by the decoder when
     *   the field is non-canonical.
     */
    int want_opc = instr_get_opcode(want);
    if ((want_opc == OP_sys || want_opc == OP_sysl) &&
        instr_get_opcode(&got) != want_opc && instr_get_opcode(&got) != OP_xx) {
        instr_disassemble_to_buffer(GD, want, buf1, sizeof(buf1));
        instr_disassemble_to_buffer(GD, &got, buf2, sizeof(buf2));
        REPLAY_CHECK(sys_alias_matches(want, enc),
                   "template %08x: encoded %08x is not the requested system "
                   "operation\n  requested: %s\n  decoded:   %s",
                   template_word, enc, buf1, buf2);
        sys_aliases++;
        instr_free(GD, &got);
        return;
    }
    if ((want_opc == OP_and || want_opc == OP_ands || want_opc == OP_eor ||
         want_opc == OP_orr) &&
        instr_num_srcs(want) == 3 && instr_num_srcs(&got) == 2 &&
        instr_num_dsts(want) == 1 && opnd_is_immed_int(instr_get_src(want, 1))) {
        /* reset_operands() doesn't preserve the operands. */
        opnd_t dst0 = instr_get_dst(want, 0), src0 = instr_get_src(want, 0),
               src1 = instr_get_src(want, 1);
        reset_operands(want, 1, 2);
        instr_set_dst(want, 0, dst0);
        instr_set_src(want, 0, src0);
        instr_set_src(want, 1, src1);
    }
    instr_disassemble_to_buffer(GD, want, buf1, sizeof(buf1));
    instr_disassemble_to_buffer(GD, &got, buf2, sizeof(buf2));
    REPLAY_CHECK(instr_get_opcode(&got) == instr_get_opcode(want),
               "template %08x: encoded %08x decodes as a different opcode\n"
               "  requested: %s\n  decoded:   %s",
               template_word, enc, buf1, buf2);
    REPLAY_CHECK(instr_num_srcs(&got) == instr_num_srcs(want) &&
                   instr_num_dsts(&got) == instr_num_dsts(want),
               "template %08x: encoded %08x decodes with a different operand count\n"
               "  requested: %s\n  decoded:   %s",
               template_word, enc, buf1, buf2);
    for (int i = 0; i < instr_num_dsts(want); i++) {
        const char *diff = opnd_mismatch(instr_get_opcode(want), instr_get_dst(want, i),
                                         instr_get_dst(&got, i));
        REPLAY_CHECK(diff == NULL,
                   "template %08x: encoded %08x changed dst %d (%s)\n"
                   "  requested: %s\n  decoded:   %s",
                   template_word, enc, i, diff, buf1, buf2);
    }
    for (int i = 0; i < instr_num_srcs(want); i++) {
        const char *diff = opnd_mismatch(instr_get_opcode(want), instr_get_src(want, i),
                                         instr_get_src(&got, i));
        REPLAY_CHECK(diff == NULL,
                   "template %08x: encoded %08x changed src %d (%s)\n"
                   "  requested: %s\n  decoded:   %s",
                   template_word, enc, i, diff, buf1, buf2);
    }
    instr_free(GD, &got);
}

int
replay_input(const uint8_t *data, size_t size)
{
    static bool registered;
    if (!registered) {
        registered = true;
        atexit(report_counts);
    }
    if (size < HEADER_SIZE)
        return 0;
    replay_input_t in = { data, size };
    replay_set_vl(replay_u8(&in));
    const uint8_t flags = replay_u8(&in);
    byte *pc = (byte *)(ptr_uint_t)(replay_u64(&in) & ~(uint64)3);
    uint word = replay_u32(&in);

    instr_t instr;
    instr_init(GD, &instr);
    int pop = (flags & 1) != 0    ? POP_SCRATCH
        : in.size < MUTATION_SIZE ? POP_UNMUTATED
                                  : POP_MUTATED;
    if ((flags & 3) == 2) {
        /* The valid population: rename registers and nothing else. */
        decode_from_copy(GD, (byte *)&word, pc, &instr);
        int opc = instr_get_opcode(&instr);
        if (opc == OP_xx || rename_exempt(opc) || !gprs_all_encoded(&instr, word)) {
            instr_free(GD, &instr);
            return 0;
        }
        int rot = 2 * (1 + replay_u8(&in) % 14);
        for (int i = 0; i < instr_num_dsts(&instr); i++)
            instr_set_dst(&instr, i, rename_opnd(instr_get_dst(&instr, i), rot));
        for (int i = 0; i < instr_num_srcs(&instr); i++)
            instr_set_src(&instr, i, rename_opnd(instr_get_src(&instr, i), rot));
        replay_check_instr_disassembly(&instr, 256);
        check_roundtrip(&instr, pc, word, POP_RENAMED);
        instr_free(GD, &instr);
        return 0;
    }
    if ((flags & 1) != 0) {
        /* From scratch: a real opcode (not the OP_xx raw-word placeholder or
         * OP_ldstex), then operands of arbitrary kinds.
         */
        instr_set_opcode(&instr, source_opcode(word));
        const int ndst = (word >> 24) & 3, nsrc = (word >> 26) & 7;
        instr_set_num_opnds(GD, &instr, ndst, nsrc);
        for (int i = 0; i < ndst + nsrc; i++) {
            const uint8_t kind = replay_u8(&in);
            const opnd_t op = fresh_operand(kind, replay_u64(&in), pc);
            if (i < ndst)
                instr_set_dst(&instr, i, op);
            else
                instr_set_src(&instr, i - ndst, op);
        }
    } else {
        decode_from_copy(GD, (byte *)&word, pc, &instr);
        if (instr_get_opcode(&instr) == OP_xx) {
            instr_free(GD, &instr);
            return 0;
        }
    }

    for (int m = 0; m < MAX_MUTATIONS && in.size >= MUTATION_SIZE; m++) {
        uint8_t target = replay_u8(&in);
        uint8_t kind = replay_u8(&in);
        uint64 payload = replay_u64(&in);
        if (kind == 0xff) {
            /* Change the operand count; encoders must not index past it. */
            int ndst = target & 3, nsrc = (target >> 2) & 7;
            opnd_t keep_dst[4], keep_src[8];
            for (int i = 0; i < ndst; i++) {
                keep_dst[i] = i < instr_num_dsts(&instr)
                    ? instr_get_dst(&instr, i)
                    : opnd_create_reg(pick_reg(payload));
            }
            for (int i = 0; i < nsrc; i++) {
                keep_src[i] = i < instr_num_srcs(&instr)
                    ? instr_get_src(&instr, i)
                    : opnd_create_immed_int((ptr_int_t)interesting_value(payload, 0),
                                            OPSZ_8);
            }
            reset_operands(&instr, ndst, nsrc);
            for (int i = 0; i < ndst; i++)
                instr_set_dst(&instr, i, keep_dst[i]);
            for (int i = 0; i < nsrc; i++)
                instr_set_src(&instr, i, keep_src[i]);
            continue;
        }
        bool is_dst = (target & 0x80) != 0;
        int count = is_dst ? instr_num_dsts(&instr) : instr_num_srcs(&instr);
        if (count == 0)
            continue;
        int idx = (target & 0x7f) % count;
        opnd_t old = is_dst ? instr_get_dst(&instr, idx) : instr_get_src(&instr, idx);
        opnd_t neu = mutate(old, kind, payload, pc);
        if (is_dst)
            instr_set_dst(&instr, idx, neu);
        else
            instr_set_src(&instr, idx, neu);
    }

    replay_check_instr_disassembly(&instr, 256);
    check_roundtrip(&instr, pc, word, pop);
    instr_free(GD, &instr);
    return 0;
}
