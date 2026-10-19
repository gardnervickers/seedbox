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

/* Checks what DR says about decoded A64 instructions against independent
 * models of the architecture.  Tools such as drmemtrace, drcachesim and
 * memory checkers rely on these answers, and a wrong one is silent:
 *  - instr_compute_address_ex() for scalar base+index+disp addressing, against
 *    a model of the AArch64 extend/shift rules and register views;
 *  - instr_is_mov_constant() against movz/movn semantics;
 *  - register-use queries agree with each other and with the operand lists;
 *  - instr_reads_memory()/instr_writes_memory() agree with the operands;
 *  - reg_get_value_ex()/reg_set_value_ex() stay inside dr_mcontext_t (ASan)
 *    and round-trip for every register an instruction uses.
 *
 * Input layout:
 *   byte 0      SVE vector length index
 *   bytes 1-    A64 words; the (up to) MC_FUZZ_BYTES bytes after each word
 *               also seed the register file for that word: x0-x7 directly,
 *               x8-x30 and SP expanded from them (see fill_mcontext()).
 */

#include "input.h"
#include "source_selectors.h"

/* Bytes of register state the input supplies (x0-x7); the other GPRs and SP
 * are derived from them.
 */
#define MC_FUZZ_BYTES 64

static reg_t
model_gpr(dr_mcontext_t *mc, reg_id_t reg)
{
    if (reg >= DR_REG_X0 && reg <= DR_REG_X30)
        return (&mc->r0)[reg - DR_REG_X0];
    if (reg >= DR_REG_W0 && reg <= DR_REG_W30)
        return (&mc->r0)[reg - DR_REG_W0] & 0xffffffffULL;
    if (reg == DR_REG_XSP)
        return mc->sp;
    if (reg == DR_REG_WSP)
        return mc->sp & 0xffffffffULL;
    if (reg == DR_REG_XZR || reg == DR_REG_WZR || reg == DR_REG_NULL)
        return 0;
    REPLAY_CHECK(false, "model has no value for reg %s", get_register_name(reg));
    return 0;
}

/* The architectural address for a scalar base+index+disp operand. */
static app_pc
model_address(opnd_t mem, dr_mcontext_t *mc)
{
    reg_t addr = model_gpr(mc, opnd_get_base(mem));
    reg_id_t index = opnd_get_index(mem);
    if (index != DR_REG_NULL) {
        bool scaled = false;
        uint amount = 0;
        dr_extend_type_t ext = opnd_get_index_extend(mem, &scaled, &amount);
        reg_t val = model_gpr(mc, index);
        switch (ext) {
        case DR_EXTEND_UXTW: val = (uint)val; break;
        case DR_EXTEND_SXTW: val = (reg_t)(int64)(int)(uint)val; break;
        case DR_EXTEND_UXTX:
        case DR_EXTEND_SXTX: break;
        default: REPLAY_CHECK(false, "unexpected index extend %d on decoded operand", ext);
        }
        addr += scaled ? val << amount : val;
    }
    return (app_pc)(addr + (reg_t)(ptr_int_t)opnd_get_disp(mem));
}

/* A value for register r (8-31, 31 = SP) derived from the input's x0-x7.
 * Reading 31 registers from MC_FUZZ_BYTES input bytes used to leave x8-x30 and
 * SP zero, so no address through them was ever non-zero.  The derived values
 * mix the cases address computation is sensitive to: copies of other registers
 * (aliasing), zero, sign/zero extension of the low word, the top bit, inverses
 * and distinct per-register values.
 */
static reg_t
derived_reg(const uint64_t *x, int r)
{
    uint64_t v = x[r & 7], sel = (x[(r * 5) & 7] >> (r * 2)) & 7;
    switch (sel) {
    case 0: return v; /* alias of x(r % 8) */
    case 1: return ~v;
    case 2: return (reg_t)(int64_t)(int32_t)v; /* sign-extended word */
    case 3: return 0;
    case 4: return v | (1ULL << 63);
    case 5: return (uint32_t)v;                              /* zero-extended word */
    case 6: return (v << (r & 31)) | (v >> (64 - (r & 31))); /* rotate */
    default: return v * 0x9e3779b97f4a7c15ULL + (uint64_t)r;
    }
}

static void
fill_mcontext(dr_mcontext_t *mc, replay_input_t *in)
{
    uint8_t *raw = (uint8_t *)mc;
    for (size_t i = 0; i < sizeof(*mc); i++)
        raw[i] = (uint8_t)(i * 131 + 7);
    /* x0-x7 come straight from the input, as before, so saved inputs keep
     * testing the same values there.
     */
    uint64_t x[8];
    for (int i = 0; i < 8; i++) {
        x[i] = replay_u64(in);
        (&mc->r0)[i] = (reg_t)x[i];
    }
    for (int i = 8; i <= 30; i++)
        (&mc->r0)[i] = derived_reg(x, i);
    mc->sp = derived_reg(x, 31);
    mc->size = sizeof(*mc);
    mc->flags = DR_MC_ALL;
}

static void
check_addresses(instr_t *instr, dr_mcontext_t *mc)
{
    char buf[256];
    /* instr_compute_address_ex() numbers memory operands dsts first, then srcs. */
    uint index = 0;
    for (int pass = 0; pass < 2; pass++) {
        int n = pass == 0 ? instr_num_dsts(instr) : instr_num_srcs(instr);
        for (int i = 0; i < n; i++) {
            opnd_t op = pass == 0 ? instr_get_dst(instr, i) : instr_get_src(instr, i);
            if (!opnd_is_memory_reference(op))
                continue;
            if (opnd_is_vector_base_disp(op))
                return; /* Handled by the decode harness's NULL/overflow checks. */
            app_pc addr = NULL;
            bool is_write = false;
            bool ok = instr_compute_address_ex(instr, mc, index, &addr, &is_write);
            if (pass == 1 && !ok)
                return; /* e.g. prefetches are not reported as reads. */
            instr_disassemble_to_buffer(GD, instr, buf, sizeof(buf));
            REPLAY_CHECK(ok, "no address for memory operand %u of %s", index, buf);
            REPLAY_CHECK(is_write == (pass == 0), "operand %u of %s: is_write=%d", index,
                       buf, is_write);
            app_pc want =
                opnd_is_rel_addr(op) ? opnd_get_addr(op) : model_address(op, mc);
            REPLAY_CHECK(addr == want, "%s: memory operand %u at %p, expected %p", buf,
                       index, addr, want);
            index++;
        }
    }
}

static void
check_mov_constant(instr_t *instr)
{
    ptr_int_t value;
    if (!instr_is_mov_constant(instr, &value))
        return;
    int opc = instr_get_opcode(instr);
    if (opc != OP_movz && opc != OP_movn)
        return;
    char buf[256];
    instr_disassemble_to_buffer(GD, instr, buf, sizeof(buf));
    /* movz/movn dst <- #imm16, lsl, #shift (see codec_v80.txt). */
    REPLAY_CHECK(instr_num_srcs(instr) == 3 && opnd_is_immed_int(instr_get_src(instr, 0)) &&
                   opnd_is_immed_int(instr_get_src(instr, 2)),
               "unexpected movz/movn operands: %s", buf);
    uint64 imm = (uint64)opnd_get_immed_int(instr_get_src(instr, 0)) & 0xffff;
    uint shift = (uint)opnd_get_immed_int(instr_get_src(instr, 2));
    uint64 val = imm << shift;
    if (opc == OP_movn)
        val = ~val;
    reg_id_t dst = opnd_get_reg(instr_get_dst(instr, 0));
    if (reg_get_size(dst) == OPSZ_4)
        val &= 0xffffffffULL;
    REPLAY_CHECK((uint64)value == val,
               "instr_is_mov_constant(%s) = 0x%llx, expected 0x%llx", buf,
               (unsigned long long)value, (unsigned long long)val);
}

static void
check_reg_queries(instr_t *instr)
{
    char buf[256];
    instr_disassemble_to_buffer(GD, instr, buf, sizeof(buf));
    for (unsigned id = 2; id < SOURCE_REG_COUNT; ++id) {
        reg_id_t reg = source_reg(id);
        bool in_src = instr_reg_in_src(instr, reg);
        bool in_dst = instr_reg_in_dst(instr, reg);
        bool uses = instr_uses_reg(instr, reg);
        REPLAY_CHECK(uses == (in_src || in_dst),
                   "%s: instr_uses_reg(%s)=%d but in_src=%d in_dst=%d", buf,
                   get_register_name(reg), uses, in_src, in_dst);
    }
    /* Implicit FFR users: RDFFR reads FFR, WRFFR/SETFFR write it. */
    int opc = instr_get_opcode(instr);
    if (opc == OP_rdffr || opc == OP_rdffrs) {
        REPLAY_CHECK(instr_reads_from_reg(instr, DR_REG_FFR, DR_QUERY_DEFAULT),
                   "%s does not read FFR", buf);
    }
    if (opc == OP_wrffr || opc == OP_setffr) {
        REPLAY_CHECK(instr_writes_to_reg(instr, DR_REG_FFR, DR_QUERY_DEFAULT),
                   "%s does not write FFR", buf);
    }

    bool mem_src = false, mem_dst = false;
    for (int i = 0; i < instr_num_srcs(instr); i++)
        mem_src |= opnd_is_memory_reference(instr_get_src(instr, i));
    for (int i = 0; i < instr_num_dsts(instr); i++)
        mem_dst |= opnd_is_memory_reference(instr_get_dst(instr, i));
    REPLAY_CHECK(!instr_writes_memory(instr) || mem_dst,
               "%s: writes memory without a memory dst", buf);
    /* STG/ST2G/STGM store only allocation tags, which DR deliberately does not
     * count as memory writes.
     */
    opc = instr_get_opcode(instr);
    bool tag_only_store = opc == OP_stg || opc == OP_st2g || opc == OP_stgm;
    REPLAY_CHECK(!mem_dst || tag_only_store || instr_writes_memory(instr),
               "%s: memory dst but instr_writes_memory is false", buf);
    REPLAY_CHECK(!instr_reads_memory(instr) || mem_src,
               "%s: reads memory without a memory src", buf);
}

static void
check_reg_values(instr_t *instr, dr_mcontext_t *mc)
{
    /* Big enough for a 2048-bit Z register. */
    byte val[512], back[512];
    for (int pass = 0; pass < 2; pass++) {
        int n = pass == 0 ? instr_num_dsts(instr) : instr_num_srcs(instr);
        for (int i = 0; i < n; i++) {
            opnd_t op = pass == 0 ? instr_get_dst(instr, i) : instr_get_src(instr, i);
            for (int r = 0; r < opnd_num_regs_used(op); r++) {
                reg_id_t reg = opnd_get_reg_used(op, r);
                memset(val, 0x5a, sizeof(val));
                if (!reg_get_value_ex(reg, mc, val))
                    continue;
                if (!reg_set_value_ex(reg, mc, val))
                    continue;
                memset(back, 0, sizeof(back));
                REPLAY_CHECK(reg_get_value_ex(reg, mc, back), "re-get of %s failed",
                           get_register_name(reg));
                size_t bytes = opnd_size_in_bytes(reg_get_size(reg));
                if (bytes > sizeof(val))
                    bytes = sizeof(val);
                REPLAY_CHECK(memcmp(val, back, bytes) == 0, "set/get of %s not identity",
                           get_register_name(reg));
                /* reg_{get,set}_value() are documented for pointer-sized GPRs
                 * only.  (reg_is_pointer_sized() is also true for Z registers
                 * on AArch64: i#6750.)
                 */
                if (reg >= DR_REG_X0 && reg <= DR_REG_XZR)
                    reg_set_value(reg, mc, reg_get_value(reg, mc));
            }
        }
    }
}

int
replay_input(const uint8_t *data, size_t size)
{
    if (size < 5)
        return 0;
    replay_input_t in = { data, size };
    replay_set_vl(replay_u8(&in));
    /* Exact-size heap block so ASan sees accesses past the end. */
    dr_mcontext_t *mc = (dr_mcontext_t *)malloc(sizeof(*mc));
    while (in.size >= 4) {
        uint word = replay_u32(&in);
        replay_input_t regs = { in.data,
                              in.size < MC_FUZZ_BYTES ? in.size : MC_FUZZ_BYTES };
        fill_mcontext(mc, &regs);
        byte *pc = (byte *)(ptr_uint_t)(0x400000 + 4 * (in.size & 0xffff));

        instr_t instr;
        instr_init(GD, &instr);
        decode_from_copy(GD, (byte *)&word, pc, &instr);
        if (instr_get_opcode(&instr) != OP_xx) {
            check_addresses(&instr, mc);
            check_mov_constant(&instr);
            check_reg_queries(&instr);
            check_reg_values(&instr, mc);
        }
        instr_free(GD, &instr);
    }
    free(mc);
    return 0;
}
