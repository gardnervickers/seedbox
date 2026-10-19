/* Exact source client scenario, public ABI alignment check replaces private client_tools.h. */
/* **********************************************************
 * Copyright (c) 2018-2020 Google, Inc.  All rights reserved.
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
 * * Neither the name of VMware, Inc. nor the names of its contributors may be
 *   used to endorse or promote products derived from this software without
 *   specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL VMWARE, INC. OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
 * DAMAGE.
 */

#include "dr_api.h"
static app_pc lo[16],hi[16];static uint segments;static int app_calls;

static void
cleancallee(app_pc pc)
{
    /* Include enough complexity to avoid inlining. */
    ptr_uint_t sp;
#ifdef AARCH64
    __asm__ volatile("mov %0,sp":"=r"(sp));
#else
    __asm__ volatile("mov %%rsp,%0":"=r"(sp));
#endif
    DR_ASSERT((sp&15)==0);
    for(uint i=0;i<segments;i++)if(pc>=lo[i] && pc<hi[i]) {dr_atomic_add32_return_sum(&app_calls,1);break;}
    if (pc == NULL) {
        dr_fprintf(STDERR, "pc is NULL\n");
    }
}

static dr_emit_flags_t
bb_event(void *drcontext, void *tag, instrlist_t *bb, bool for_trace, bool translating)
{
    static int64 bb_count;
    if (++bb_count % 10 == 0) {
        for (instr_t *inst = instrlist_first(bb); inst != NULL;
             inst = instr_get_next(inst)) {
            if (instr_is_exclusive_store(inst))
                return DR_EMIT_DEFAULT;
        }
        dr_insert_clean_call(drcontext, bb, instrlist_first(bb), (void *)cleancallee,
                             false, 1, OPND_CREATE_INTPTR(tag));
    }
    return DR_EMIT_DEFAULT;
}

static void finished(void) {DR_ASSERT(app_calls>0);dr_fprintf(STDERR,"unix-cleancallsig: app-clean-calls=%d\n",app_calls);}
DR_EXPORT void
dr_client_main(client_id_t id,int argc,const char *argv[])
{
    (void)id;(void)argc;(void)argv;
    module_data_t *main=dr_get_main_module();DR_ASSERT(main && main->num_segments<=16);
    for(uint i=0;i<main->num_segments;i++)if(main->segments[i].prot&DR_MEMPROT_EXEC) {lo[segments]=main->segments[i].start;hi[segments++]=main->segments[i].end;}
    dr_free_module_data(main);dr_register_exit_event(finished);
    dr_register_bb_event(bb_event);
}
