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

/* Application-only capture/comparison components extracted from source
 *356c1821045a4b5678d0813e132074324623df6e a64_prop_support.h. No upstream
 * fork supervisor, static-DR orchestration, ptrace runner or calibrator. */

#ifndef A64_PROP_SUPPORT_H
#define A64_PROP_SUPPORT_H

#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/types.h>
#include <time.h>
#include <ucontext.h>

#include "state_generator.h"

/***************************************************************************
 * Captured state.
 */
typedef struct {
    int signo, code;
    uint64_t addr, pc, sp, pstate, fault_address, esr;
    int has_esr, has_fp;
    uint64_t x[31];
    uint32_t fpsr, fpcr;
    __uint128_t v[32];
} fault_rec_t;

/* Fault frames recorded and compared per run: every one.  The generator aims
 * for at most about 24 dynamic faults but underestimates loops (instr seed
 * 346 has 41); a run with more than MAXF is a harness failure
 * (a64p_check_planned() rejects it natively, and the fault count comparison
 * rejects it under DR).
 */
#define MAXF 64
/* Faults after which the sync handler abandons the block. */
#define MAXF_RUN 64

typedef struct {
    cpu_state_t out;
    uint8_t d[D_SIZE];
    uint8_t stk[STK_CMP];
    int nf;
    fault_rec_t f[MAXF];
} result_t;

/* One asynchronous signal's interrupted context. */
typedef struct {
    int sig, run, applied;
    uint64_t pc, sp, pstate;
    uint64_t x[31];
    uint32_t fpsr, fpcr;
    uint64_t vhash, dhash, vres_lo;
} arec_t;
#define MAXA 8192

/***************************************************************************
 * Per-worker execution state.
 */
typedef struct {
    /* Read by the signal handlers. */
    volatile int in_block;
    volatile uint64_t blk_lo, blk_hi, exit_pc, code_hi;
    volatile int nf;
    fault_rec_t flog[MAXF];
    /* Asynchronous signal log. */
    arec_t *alog;
    volatile int na;
    volatile int async_on;
    volatile int xres, vres;
    volatile uint64_t delta_sum;
    /* Code installation. */
    int code_mapped_mode;
    harn_t *harn;
    stack_t altstk;
    /* The generator layout slot and its bases (a64g_slot_*_base()). */
    int slot;
    uint64_t code_base, data_base;
} a64p_worker_t;

/* Allocates the worker's logs; the worker uses layout slot 0. */
void
a64p_worker_init(a64p_worker_t *w);

/* Like a64p_worker_init() but for layout slot slot (0..A64G_MAX_SLOTS-1). */
void
a64p_worker_init_slot(a64p_worker_t *w, int slot);

/* Makes w the worker the signal handlers on this thread use, and selects w's
 * layout slot for the generator and the support module on this thread.
 */
void
a64p_worker_bind(a64p_worker_t *w);

/***************************************************************************
 * Setup, installation and running.
 */
/* Fills in the generator's CPU features from HWCAPs; returns whether DC ZVA
 * has the 64-byte block size the generator assumes.
 */
int
a64p_init_features(void);

/* Maps the data region (data buffer, guard page, stack, harness) of the
 * calling thread's layout slot.
 */
void
a64p_setup_maps(void);

void
a64p_install_handlers(a64p_worker_t *w, int altstack);

void
a64p_sync_icache(uint64_t start, uint64_t len, int honour_dic);

void
a64p_install_code(a64p_worker_t *w, block_t *b, int mode);

void
a64p_prepare_run(a64p_worker_t *w, block_t *b);

void
a64p_run_block(a64p_worker_t *w, block_t *b, result_t *res);

/* Generates b from seed, retrying with derived seeds. */
int
a64p_make_block(block_t *b, const gen_opts_t *o, uint64_t seed);

/* Host-side entry trampoline (in a64_prop_support.c): sets h->run_id to run
 * for exactly the time between its first and last instruction.
 */
void
run_code(uint64_t entry, harn_t *h, int64_t run);
extern const char run_code_end[];

/* The current run id, or -1 between runs. */
static inline int
a64p_run_id(a64p_worker_t *w)
{
    return (int)__atomic_load_n(&w->harn->run_id, __ATOMIC_ACQUIRE);
}

/* Whether pc is host code that legitimately runs inside a run: the
 * trampoline itself.  (The generated code's entry/exit stubs are in
 * [CODE_BASE, code_hi).)
 */
static inline int
a64p_pc_in_trampoline(uint64_t pc)
{
    return pc >= (uint64_t)(uintptr_t)run_code && pc < (uint64_t)(uintptr_t)run_code_end;
}

/* Runs b with h->run_id = run. */
void
a64p_run_block_id(a64p_worker_t *w, block_t *b, result_t *res, int run);

/***************************************************************************
 * Comparison and reporting.
 */
/* PSTATE bits ignored when comparing fault frames.  When native and DR frames
 * differ only in ignored bits, a64p_on_masked_pstate (if set) is called, so
 * that the test can count and report each masked difference.
 */
extern uint64_t a64p_fault_pstate_ignore;
extern void (*a64p_on_masked_pstate)(int i, fault_rec_t *n, fault_rec_t *d);

/* If set, called by the synchronous handler after it records fault n of a
 * run in the block, before it resumes: it may change the frame (e.g., to
 * perturb a register to test whether it is live).
 */
extern void (*a64p_fault_hook)(a64p_worker_t *w, int n, ucontext_t *uc);

int
a64p_cmp_fault(FILE *o, int i, fault_rec_t *n, fault_rec_t *d);

int
a64p_cmp_state(FILE *o, const char *what, cpu_state_t *n, cpu_state_t *d, int xres,
               uint64_t xadj, int vres, uint64_t vadj);

int
a64p_cmp_mem(FILE *o, block_t *b, result_t *n, result_t *d);

int
a64p_cmp_results(FILE *o, block_t *b, result_t *n, result_t *d);

void
a64p_print_faults(FILE *o, const char *who, result_t *r);

/* Faults the generator did not plan are generator bugs, not DR bugs; so are
 * more faults than a run can record.
 */
int
a64p_check_planned(block_t *b, result_t *r);


#endif
