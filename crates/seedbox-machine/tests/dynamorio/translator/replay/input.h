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

/* Finite saved-input replay support. No fuzz entry point, corpus search,
 * assertion hook, known-bug filter, or private state. Every input is a fresh
 * process; diagnostics name the original saved file before any API call. */
#ifndef TRANSLATOR_REPLAY_H
#define TRANSLATOR_REPLAY_H
#include "dr_api.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef DYNAMORIO_STANDALONE
static void *replay_context;
# define GD replay_context
#else
# define GD GLOBAL_DCONTEXT
#endif
#    define REPLAY_CHECK(cond, ...)                                                       \
        do {                                                                            \
            if (!(cond)) {                                                              \
                fprintf(stderr, "REPLAY_CHECK failed: %s:%d: %s\n  ", __FILE__, __LINE__, \
                        #cond);                                                         \
                fprintf(stderr, __VA_ARGS__);                                           \
                fprintf(stderr, "\n");                                                  \
                abort();                                                                \
            }                                                                           \
        } while (0)

typedef struct {
    const uint8_t *data;
    size_t size;
} replay_input_t;

static inline uint8_t
replay_u8(replay_input_t *in)
{
    if (in->size == 0)
        return 0;
    uint8_t v = in->data[0];
    in->data++;
    in->size--;
    return v;
}

static inline uint32_t
replay_u32(replay_input_t *in)
{
    uint32_t v = 0;
    for (int i = 0; i < 4; i++)
        v |= (uint32_t)replay_u8(in) << (8 * i);
    return v;
}

static inline uint64_t
replay_u64(replay_input_t *in)
{
    uint64_t lo = replay_u32(in);
    return lo | ((uint64_t)replay_u32(in) << 32);
}

static inline void
replay_check_instr_disassembly(instr_t *instr, size_t bufsz)
{
    const size_t slack = 16;
    char *allocation = (char *)malloc(bufsz + 2 * slack);
    REPLAY_CHECK(allocation != NULL, "disassembly buffer allocation");
    memset(allocation, 0xa5, bufsz + 2 * slack);
    char *buf = allocation + slack;
    size_t len = instr_disassemble_to_buffer(GD, instr, buf, bufsz);
    if (bufsz > 0) {
        REPLAY_CHECK(len < bufsz, "instr_disassemble_to_buffer returned %zu for bufsz %zu",
                   len, bufsz);
        REPLAY_CHECK(buf[len] == '\0', "disassembly not NUL-terminated at %zu", len);
        REPLAY_CHECK(strlen(buf) == len, "strlen %zu != reported %zu", strlen(buf), len);
    }
    for (size_t i = bufsz; i < bufsz + slack; i++) {
        REPLAY_CHECK((uint8_t)buf[i] == 0xa5, "disassembly wrote byte %zu past bufsz %zu",
                   i - bufsz, bufsz);
    }
    for (size_t i = 0; i < slack; ++i)
        REPLAY_CHECK((uint8_t)allocation[i] == 0xa5, "disassembly wrote before buffer");
    free(allocation);
}


static int replay_set_vl(int index)
{
    int vl = 128 * (1 + index % 16);
    REPLAY_CHECK(dr_set_vector_length(vl), "vector length %d", vl);
    return vl;
}
int replay_input(const uint8_t *data, size_t size);
int main(int argc, char **argv)
{
#ifdef DYNAMORIO_STANDALONE
    replay_context = dr_standalone_init();
    if (replay_context == NULL)
        return 2;
    atexit(dr_standalone_exit);
#endif
    REPLAY_CHECK(argc == 2, "expected one saved input path");
    fprintf(stderr, "saved-input: %s\n", argv[1]);
    FILE *f = fopen(argv[1], "rb");
    REPLAY_CHECK(f != NULL, "open saved input");
    REPLAY_CHECK(fseek(f, 0, SEEK_END) == 0, "seek");
    long length = ftell(f);
    REPLAY_CHECK(length > 0, "empty input");
    rewind(f);
    uint8_t *bytes = malloc((size_t)length);
    REPLAY_CHECK(bytes != NULL, "allocation");
    REPLAY_CHECK(fread(bytes, 1, (size_t)length, f) == (size_t)length, "read");
    REPLAY_CHECK(fclose(f) == 0, "close");
    dr_set_isa_mode(GD, DR_ISA_ARM_A64, NULL);
    int result = replay_input(bytes, (size_t)length);
    free(bytes);
    REPLAY_CHECK(result == 0, "input rejected");
    printf("saved-input: PASS %s\n", argv[1]);
    return 0;
}
#endif
