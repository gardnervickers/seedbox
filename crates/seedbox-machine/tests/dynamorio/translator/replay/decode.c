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

/* Decodes arbitrary A64 words and checks what the rest of the IR does with
 * the result:
 *  - decode never fails on A64 (unknown words become OP_xx) and must return
 *    pc + 4 with an opcode in range;
 *  - disassembly stays inside the caller's buffer at every buffer size;
 *  - operand/instr query routines and address computation (including SVE
 *    scatter/gather against a fuzzed machine context) don't misbehave;
 *  - re-encoding a decoded instruction either fails cleanly or yields bits
 *    that decode back to the same instruction (instr_same()), both at the
 *    original pc and at a fuzzed pc that moves pc-relative targets.
 *
 * Input layout:
 *   byte 0      SVE vector length index into replay_sve_vls[]
 *   byte 1      disassembly syntax selector
 *   byte 2      small disassembly buffer size
 *   byte 3      flags (bit 0: check re-encoding at a second pc)
 *   bytes 4-11  original pc of the first word
 *   bytes 12-19 delta to the second encode pc
 *   bytes 20-   A64 words, little-endian
 */

#include "input.h"

#define HEADER_SIZE 20

/* Exercise both public syntaxes, including the original ARM syntax regressions. */
static const dr_disasm_flags_t syntaxes[] = {
    DR_DISASM_DR,
    DR_DISASM_DR | DR_DISASM_NO_OPND_SIZE,
    DR_DISASM_ARM,
    DR_DISASM_ARM | DR_DISASM_NO_OPND_SIZE,
};
#define NUM_SYNTAXES (sizeof(syntaxes) / sizeof(syntaxes[0]))

static const char *
disas(instr_t *instr, char *buf, size_t bufsz)
{
    instr_disassemble_to_buffer(GD, instr, buf, bufsz);
    return buf;
}

static void
check_operand(opnd_t opnd)
{
    opnd_size_t size = opnd_get_size(opnd);
    (void)opnd_size_in_bytes(size);
    (void)opnd_size_in_bits(size);
    for (int r = 0; r < opnd_num_regs_used(opnd); r++) {
        reg_id_t reg = opnd_get_reg_used(opnd, r);
        REPLAY_CHECK(reg < (DR_REG_LAST_VALID_ENUM + 1), "reg id %d out of range", reg);
        const char *name = get_register_name(reg);
        REPLAY_CHECK(name != NULL, "no name for reg %d", reg);
        (void)reg_get_size(reg);
    }
    if (opnd_is_base_disp(opnd)) {
        bool scaled = false;
        uint amount = 0;
        (void)opnd_get_index_extend(opnd, &scaled, &amount);
        REPLAY_CHECK(amount < 64, "index shift amount %u", amount);
    }
    if (opnd_is_reg(opnd) && opnd_is_element_vector_reg(opnd)) {
        opnd_size_t elsz = opnd_get_vector_element_size(opnd);
        (void)opnd_size_in_bytes(elsz);
    }
}

static void
check_queries(instr_t *instr)
{
    for (int i = 0; i < instr_num_srcs(instr); i++)
        check_operand(instr_get_src(instr, i));
    for (int i = 0; i < instr_num_dsts(instr); i++)
        check_operand(instr_get_dst(instr, i));

    (void)instr_get_eflags(instr, DR_QUERY_DEFAULT);
    (void)instr_get_eflags(instr, DR_QUERY_INCLUDE_ALL);
    (void)instr_get_arith_flags(instr, DR_QUERY_DEFAULT);
    (void)instr_get_category(instr);
    (void)instr_reads_memory(instr);
    (void)instr_writes_memory(instr);
    (void)instr_is_cti(instr);
    (void)instr_is_cbr(instr);
    (void)instr_is_ubr(instr);
    (void)instr_is_mbr(instr);
    (void)instr_is_call(instr);
    (void)instr_is_return(instr);
    (void)instr_is_exclusive_load(instr);
    (void)instr_is_exclusive_store(instr);
    (void)instr_is_scatter(instr);
    (void)instr_is_gather(instr);
    ptr_int_t value;
    (void)instr_is_mov_constant(instr, &value);
    (void)instr_get_predicate(instr);
    (void)instr_length(GD, instr);
    if (instr_is_cbr(instr) || instr_is_ubr(instr) || instr_is_call_direct(instr)) {
        opnd_t tgt = instr_get_target(instr);
        REPLAY_CHECK(opnd_is_pc(tgt) || opnd_is_instr(tgt),
                   "direct branch target is not a pc");
    }
}

static void
check_compute_address(instr_t *instr, replay_input_t *in)
{
    if (!instr_reads_memory(instr) && !instr_writes_memory(instr))
        return;
    /* Exact-size heap block so ASan sees reads past the end of the context. */
    dr_mcontext_t *mc = (dr_mcontext_t *)malloc(sizeof(*mc));
    uint8_t *raw = (uint8_t *)mc;
    for (size_t i = 0; i < sizeof(*mc); i++)
        raw[i] = (uint8_t)(i * 131 + 7);
    /* Fuzz the predicate registers so scatter/gather masks vary. */
    for (int p = 0; p < 16; p++)
        mc->svep[p].u64[0] = replay_u64(in);
    mc->size = sizeof(*mc);
    mc->flags = DR_MC_ALL;

    app_pc addr;
    bool is_write;
    for (uint index = 0; index < 512; index++) {
        if (!instr_compute_address_ex(instr, mc, index, &addr, &is_write))
            break;
        /* The API allows NULL OUT parameters. */
        {
            REPLAY_CHECK(instr_compute_address_ex(instr, mc, index, NULL, NULL),
                       "computing address %u with NULL out-params failed", index);
        }
    }
    free(mc);
}

static void
check_reencode(instr_t *instr, byte *orig_pc, byte *final_pc, uint orig_word)
{
    char buf1[256], buf2[256];
    uint enc = 0;
    instr_set_raw_bits_valid(instr, false);
    byte *end = instr_encode_to_copy(GD, instr, (byte *)&enc, final_pc);
    if (end == NULL) {
        /* An instruction the decoder produced at its own pc must re-encode.
         * At a different pc, a pc-relative target may legitimately be out of
         * range.
         */
        REPLAY_CHECK(final_pc != orig_pc || instr_get_opcode(instr) == OP_xx,
                   "decoded %08x but cannot re-encode: %s", orig_word,
                   disas(instr, buf1, sizeof(buf1)));
        return;
    }
    REPLAY_CHECK(end == (byte *)&enc + 4, "encode returned length %td", end - (byte *)&enc);

    instr_t again;
    instr_init(GD, &again);
    byte *next = decode_from_copy(GD, (byte *)&enc, final_pc, &again);
    REPLAY_CHECK(next == (byte *)&enc + 4, "re-decode of %08x failed", enc);
    REPLAY_CHECK(instr_same(instr, &again),
               "%08x @%p re-encoded as %08x @%p which decodes differently:\n"
               "  orig: %s\n  new:  %s",
               orig_word, orig_pc, enc, final_pc, disas(instr, buf1, sizeof(buf1)),
               disas(&again, buf2, sizeof(buf2)));
    instr_free(GD, &again);
}

int
replay_input(const uint8_t *data, size_t size)
{
    if (size < HEADER_SIZE + 4)
        return 0;
    replay_input_t hdr = { data, HEADER_SIZE };
    replay_set_vl(replay_u8(&hdr));
    uint8_t syntax = replay_u8(&hdr);
    disassemble_set_syntax(
        syntaxes[syntax % NUM_SYNTAXES]);
    size_t small_bufsz = replay_u8(&hdr) % 64;
    uint8_t flags = replay_u8(&hdr);
    /* A64 instructions are 4-byte aligned. */
    byte *orig_pc = (byte *)(ptr_uint_t)(replay_u64(&hdr) & ~(uint64)3);
    ptr_uint_t delta = (ptr_uint_t)(replay_u64(&hdr) & ~(uint64)3);

    /* The words double as the source of mcontext bytes for address checks. */
    replay_input_t words = { data + HEADER_SIZE, size - HEADER_SIZE };
    size_t num_words = words.size / 4;
    for (size_t w = 0; w < num_words; w++) {
        uint word;
        memcpy(&word, words.data + 4 * w, sizeof(word));
        byte *pc = (byte *)((ptr_uint_t)orig_pc + 4 * w);

        instr_t instr;
        instr_init(GD, &instr);
        byte *next = decode_from_copy(GD, (byte *)&word, pc, &instr);
        REPLAY_CHECK(next == (byte *)&word + 4, "decode of %08x returned %p", word, next);
        int opc = instr_get_opcode(&instr);
        REPLAY_CHECK(opc >= OP_FIRST && opc <= OP_LAST, "decode of %08x gave opcode %d",
                   word, opc);

        replay_check_instr_disassembly(&instr, 256);
        replay_check_instr_disassembly(&instr, small_bufsz);

        char rawbuf[256];
        int printed = 0;
        byte *dnext = disassemble_to_buffer(GD, (byte *)&word, pc, true, true, rawbuf,
                                            sizeof(rawbuf), &printed);
        REPLAY_CHECK(dnext == (byte *)&word + 4, "disassemble_to_buffer of %08x", word);
        REPLAY_CHECK(printed >= 0 && (size_t)printed < sizeof(rawbuf),
                   "disassemble_to_buffer printed %d", printed);

        check_queries(&instr);
        replay_input_t mcbytes = { words.data, words.size };
        check_compute_address(&instr, &mcbytes);

        check_reencode(&instr, pc, pc, word);
        if ((flags & 1) != 0) {
            instr_free(GD, &instr);
            instr_init(GD, &instr);
            decode_from_copy(GD, (byte *)&word, pc, &instr);
            check_reencode(&instr, pc, (byte *)((ptr_uint_t)pc + delta), word);
        }
        instr_free(GD, &instr);
    }
    return 0;
}
