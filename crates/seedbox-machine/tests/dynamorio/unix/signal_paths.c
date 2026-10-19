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

/* Signal ABI and DR-internal-path properties of AArch64 execution under
 * (static) DynamoRIO.  Each case is a hand-written scenario template (below,
 * in assembly) with seeded parameters: initial GPRs/V registers/NZCV/DIT,
 * the signal, its source, its sigaction flags and masks, and the effects the
 * handler applies to the interrupted context.  Every case runs natively (the
 * paired oracle) and then under DR, and each run is also checked against an
 * independent model of the scenario:
 *
 *   - every handler invocation sees a permitted application context: a PC the
 *     scenario allows at that point (legal deferral of asynchronous delivery
 *     is allowed: any PC of the spin loop the scenario waits in) with exactly
 *     the modelled GPRs, SP, NZCV, DIT, V registers and FPCR, and a frame
 *     whose record list is valid by the kernel ABI (a64p_check_frame_records)
 *     and identical to the native one;
 *   - handler-written GPR, NZCV, DIT, V register, PC (a labelled continuation)
 *     and SP (moved within the live stack) take effect after sigreturn, and
 *     state the handler does not write is preserved (notably DIT);
 *   - nesting (SA_NODEFER, or a second signal raised from the handler) is
 *     delivered inside the outer handler and the inner handler's edit of the
 *     outer handler's context takes effect; sa_mask blocks a signal raised in
 *     the handler until sigreturn, which then delivers it with the restored
 *     mask; sigprocmask inside the block keeps a signal pending until it is
 *     unblocked; sigaltstack enabled and disabled from the block moves the
 *     frame onto and off the alternate stack and reports it in uc_stack and
 *     sigaltstack(); real-time payloads arrive exactly once, lowest signal
 *     first and FIFO per signal; a blocking pipe read interrupted with
 *     SA_RESTART is restarted (frame PC at the svc, x0 the fd) and without it
 *     returns EINTR (frame PC after the svc, x0 -EINTR);
 *   - synchronous signals (UDF, BRK, a load from a guard page) are taken at
 *     the faulting instruction with the native si_code, si_addr, fault_address
 *     and ESR record;
 *   - live stack above the interrupted SP (canaries) is never written;
 *   - nothing hangs (every wait is bounded; a child is killed and reported
 *     after a timeout).
 *
 * Modes:
 *   abi: the scenarios natively and under DR (default, -max_bb_instrs 3 and
 *     -disable_traces, plus -steal_reg 27 and resets at every pending signal
 *     against a debug DR).
 *   paths: the same scenarios plus an IBL-churning spin loop under an
 *     in-process client that adds drreg reservations with clobbered values,
 *     aflags clobbers, drx counters and clean calls (all of them with
 *     A64_DR_PROP_CLEANCALL=1, which one variant always sets) to every
 *     scenario instruction; then "storm": threads exiting while signals are in
 *     flight.  Four threads (the main one included) wait in read() or an
 *     unbounded ppoll() of their own empty pipe, each interrupted by its own
 *     SIGEV_THREAD_ID timer and by tgkills at random moments, while a
 *     SIGEV_THREAD timer spawns notification threads that exit, other threads
 *     with the signals unblocked are created and exit (at most -churn_max, 8,
 *     at a time), a process-directed POSIX timer runs, and another thread sends
 *     sequence-numbered process-directed rt signals one at a time: each must be
 *     delivered exactly once, every expiration of the process timer must be
 *     accounted (deliveries plus overruns, counted exactly at a cut), and no
 *     waiter may stop being interrupted for 3 s (the shape of the hang fixed by
 *     "Deliver a signal that arrives on the way into a syscall before it").
 *   storm: just the storm (-waiters N, -poker 0|1 and -churn_max N vary it).
 *
 * Usage: a64_prop_signal <abi|paths|storm> [-seed S] [-n N] [-only I]
 *        [-rounds R] [-variants a,b] [-waiters N] [-poker 0|1] [-churn_max N] [-v]
 * -n is the number of cases per scenario kind; -only runs one case index.
 *
 * Known deviation (counted as suppressed): rt-overflow-order, see
 * DR_MAX_PENDING_SIGNALS below.  Known issue: memcache-check-race, see
 * STORM_DEBUG_DR_OPS below.
 *
 * Counts are printed as generated / accepted / executed / checked /
 * suppressed / inconclusive, with floors; "all properties hold" is only
 * printed if every check and floor passed.
 */

#include "dr_api.h"
#include "drmgr.h"
#include "drreg.h"
#include "drx.h"
#include <stdint.h>
#include <string.h>
#include "signal_rng.h"
static uint64_t code_lo,code_hi;
static int g_instr_on;
static int g_cleancall_all;
static volatile uint64_t g_dyn_count;
static volatile uint64_t g_cc_count;
static uint64_t g_instr_seed = 0x5157;

static void
cc_callee(uint64_t pc, uint64_t val)
{
    /* Clobbers caller-saved GPRs and vector registers as a C callee may. */
    g_cc_count += pc ^ val;
    __asm__ volatile("movi v0.16b, #0x5a\n"
                     "movi v1.16b, #0xa5\n"
                     "movi v7.16b, #0x11\n"
                     "movi v16.16b, #0x22\n"
                     "movi v31.16b, #0x33\n"
                     "mov x9, #0x1234\n"
                     "mov x15, #0x5678\n"
                     :
                     :
                     : "v0", "v1", "v7", "v16", "v31", "x9", "x15", "memory");
}

static int
rand_reserve(void *dc, instrlist_t *bb, instr_t *where, rng_t *r, reg_id_t *out)
{
    drvector_t allowed;
    drreg_init_and_fill_vector(&allowed, false);
    int any = 0;
    for (int i = 0; i < 31; i++) {
        if (rp(r, 30) && DR_REG_X0 + i != dr_get_stolen_reg()) {
            drreg_set_vector_entry(&allowed, DR_REG_X0 + i, true);
            any = 1;
        }
    }
    drreg_status_t res =
        drreg_reserve_register(dc, bb, where, any ? &allowed : NULL, out);
    if (res != DRREG_SUCCESS && any)
        res = drreg_reserve_register(dc, bb, where, NULL, out);
    drvector_delete(&allowed);
    return res == DRREG_SUCCESS;
}

static dr_emit_flags_t
event_insert(void *dc, void *tag, instrlist_t *bb, instr_t *inst, bool for_trace,
             bool translating, void *user_data)
{
    if (!instr_is_app(inst))
        return DR_EMIT_DEFAULT;
    uint64_t pc = (uint64_t)(uintptr_t)instr_get_app_pc(inst);
    if (pc < code_lo || pc >= code_hi)
        return DR_EMIT_DEFAULT;
    /* Deterministic per pc, so translation re-creates identical code. */
    rng_t r = { g_instr_seed ^ (pc * 0x9e3779b97f4a7c15ULL) };
    drx_insert_counter_update(dc, bb, inst, (dr_spill_slot_t)(SPILL_SLOT_MAX + 1),
                              (dr_spill_slot_t)(SPILL_SLOT_MAX + 1), (void *)&g_dyn_count,
                              1, DRX_COUNTER_64BIT);
    if (rp(&r, 60)) {
        reg_id_t regs[3];
        int nr = 1 + (int)rn(&r, 3), got = 0;
        for (int i = 0; i < nr; i++) {
            if (rand_reserve(dc, bb, inst, &r, &regs[got]))
                got++;
        }
        for (int i = 0; i < got; i++) {
            instrlist_insert_mov_immed_ptrsz(
                dc, (ptr_int_t)rnext(&r), opnd_create_reg(regs[i]), bb, inst, NULL, NULL);
        }
        if (got > 0 && rp(&r, 50) &&
            drreg_reserve_aflags(dc, bb, inst) == DRREG_SUCCESS) {
            instrlist_meta_preinsert(bb, inst,
                                     INSTR_CREATE_msr(dc, opnd_create_reg(DR_REG_NZCV),
                                                      opnd_create_reg(regs[0])));
            drreg_unreserve_aflags(dc, bb, inst);
        }
        for (int i = 0; i < got; i++)
            drreg_unreserve_register(dc, bb, inst, regs[i]);
    }
    if (g_cleancall_all || rp(&r, 25)) {
        int reg = (int)rn(&r, 31);
        dr_insert_clean_call(dc, bb, inst, (void *)cc_callee, false, 2,
                             OPND_CREATE_INTPTR(pc), opnd_create_reg(DR_REG_X0 + reg));
    }
    return DR_EMIT_DEFAULT;
}

static void
client_exit(void)
{
    dr_fprintf(STDERR,"unix-paths: instructions=%llu clean-calls=%llu\n",g_dyn_count,g_cc_count);
    DR_ASSERT(g_dyn_count>0 && g_cc_count>0);
    drx_exit();
    drreg_exit();
    drmgr_exit();
}


DR_EXPORT void dr_client_main(client_id_t id,int argc,const char *argv[]) {
    (void)id;g_cleancall_all=argc>1 && !strcmp(argv[1],"all");
    module_data_t *app=dr_get_main_module();DR_ASSERT(app!=NULL);
    code_lo=(uint64_t)(uintptr_t)dr_get_proc_address(app->handle,"a64ps_code_lo");
    code_hi=(uint64_t)(uintptr_t)dr_get_proc_address(app->handle,"a64ps_code_hi");
    dr_free_module_data(app);DR_ASSERT(code_lo && code_hi>code_lo);
    drreg_options_t ops={sizeof ops,3,true};
    DR_ASSERT(drmgr_init() && drreg_init(&ops)==DRREG_SUCCESS && drx_init());
    DR_ASSERT(drmgr_register_bb_instrumentation_event(NULL,event_insert,NULL));
    dr_register_exit_event(client_exit);
}
