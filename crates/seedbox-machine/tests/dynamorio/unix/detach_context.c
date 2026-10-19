/* **********************************************************
 * Copyright (c) 2018-2026 Google, Inc.  All rights reserved.
 * Copyright (c) 2025 Arm Limited  All rights reserved.
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

/* Sentinel constants additionally Copyright (c) 2015-2025 Google, Inc.
 * Exact source356c1821045a4b5678d0813e132074324623df6e,
 * detach_state_shared.{c,h}; independent GNU assembly uses only application
 * registers/layout and public DR APIs, no internal asm/harness header. */

#define _GNU_SOURCE
#include "dr_api.h"
#include "dr_app.h"
#include "support.h"
#include <assert.h>
#include <limits.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <string.h>
#include <sys/mman.h>
#include <stdatomic.h>

uint64_t detach_gpr[31],detach_status[3],detach_original_fp[3],detach_redzone[2];
unsigned char detach_vector_expect[32][16] __attribute__((aligned(16)));
unsigned char detach_vector_got[32][16] __attribute__((aligned(16)));
uintptr_t detach_saved_stack,detach_safe_stack,detach_stack_observed;
_Atomic int detach_ready,detach_progress,detach_released;
static sem_t prepared,start;
static int native_profile,plain_profile,kind,failures;
void detach_gprs_cache(void);extern char detach_gprs_cache_end[];
void detach_gprs_selfmod1(void);extern char detach_gprs_selfmod1_end[];
void detach_gprs_selfmod2(void);extern char detach_gprs_selfmod2_end[];
void detach_flags_cache(void);extern char detach_flags_cache_end[];
void detach_flags_selfmod(void);extern char detach_flags_selfmod_end[];
void detach_sp_cache(void);extern char detach_sp_cache_end[];
void detach_sp_selfmod(void);extern char detach_sp_selfmod_end[];
static struct {const char *name;void (*run)(void);char *end;} cases[]={
{"gprs_cache",detach_gprs_cache,detach_gprs_cache_end},
{"gprs_selfmod1",detach_gprs_selfmod1,detach_gprs_selfmod1_end},
{"gprs_selfmod2",detach_gprs_selfmod2,detach_gprs_selfmod2_end},
{"flags_cache",detach_flags_cache,detach_flags_cache_end},
{"flags_selfmod",detach_flags_selfmod,detach_flags_selfmod_end},
{"sp_cache",detach_sp_cache,detach_sp_cache_end},
{"sp_selfmod",detach_sp_selfmod,detach_sp_selfmod_end},
};
#if defined(__x86_64__)
static const uint64_t expected_gpr[]={0x12345678abcdef01,0x2345678abcdef012,0x345678abcdef0123,0x45678abcdef01234,0x5678abcdef012345,0x678abcdef0123456,0x78abcdef01234567,0x1234567890abcdef,0xf1234567890abcde,0xef1234567890abcd,0xdef1234567890abc,0xcdef1234567890ab,0xbcdef1234567890a,0xabcdef1234567890,0xabcdef123456789};
#else
static const uint64_t expected_gpr[]={0x123456789abcdef,0x123456789abcdef0,0x23456789abcdef01,0x3456789abcdef012,0x456789abcdef0123,0x56789abcdef01245,0x6789abcdef012345,0x789abcdef0123456,0x89abcdef01234567,0x9abcdef012345678,0xabcdef0123456789,0xbcdef0123456789a,0xcdef0123456789ab,0xdef0123456789abc,0xef0123456789abcd,0xf0123456789abcde,0xf123456789abcdef,0xf23456789abcdef0,0xf3456789abcdef01,0xf456789abcdef012,0xf56789abcdef0123,0xf6789abcdef01245,0xf789abcdef012345,0xf89abcdef0123456,0xf9abcdef01234567,0xfabcdef012345678,0xfbcdef0123456789,0xfcdef0123456789a,0xfdef0123456789ab,0xfef0123456789abc,0xff0123456789abcd};
#endif
static void *load_from;
/* How many blocks used each emit flavor (both must, see event_bb()). */
static volatile int num_emit_default, num_emit_store_translations,asm_blocks;

static dr_emit_flags_t
event_bb(void *drcontext, void *tag, instrlist_t *bb, bool for_trace, bool translating)
{
    instr_t *app_first=instrlist_first_app(bb);
    app_pc pc=app_first?instr_get_app_pc(app_first):NULL;
    if(pc>=(app_pc)cases[kind].run&&pc<(app_pc)cases[kind].end)
        dr_atomic_add32_return_sum(&asm_blocks,1);
#ifdef X86
    /* Test i#5786 where a tool-inserted pc-relative load that doesn't reach is
     * mangled by DR and the resulting DR-mangling with no translation triggers
     * DR's (fragile) clean call identication and incorrectly tries to restore
     * state from (garbage) on the dstack.
     */
    instr_t *first = instrlist_first(bb);
    dr_save_reg(drcontext, bb, first, DR_REG_XAX, SPILL_SLOT_1);
    instrlist_meta_preinsert(bb, first,
                             XINST_CREATE_load(drcontext, opnd_create_reg(DR_REG_XAX),
                                               OPND_CREATE_ABSMEM(load_from, OPSZ_PTR)));
    dr_restore_reg(drcontext, bb, first, DR_REG_XAX, SPILL_SLOT_1);
    /* XXX i#4232: If we do not give translations, detach can fail as there are so
     * many no-translation instrs here a thread can end up never landing on a
     * full-state-translatable PC.  Thus, we set the translation, and provide a
     * restore-state event.  i#4232 covers are more automated/convenient way to solve
     * this.
     */
    for (instr_t *inst = instrlist_first(bb); inst != first; inst = instr_get_next(inst))
        instr_set_translation(inst, instr_get_app_pc(first));
#endif
    /* Test both by alternating and assuming we hit both at the detach points
     * in the various tests.  Tags are instruction-aligned, so skip the bits that
     * are always zero (AArch64 tags are 4-byte aligned: tag % 2 was always 0).
     */
    ptr_uint_t bits = (ptr_uint_t)tag;
#ifdef AARCH64
    bits >>= 2;
#endif
    if (bits % 2 == 0) {
        __atomic_fetch_add(&num_emit_default, 1, __ATOMIC_RELAXED);
        return DR_EMIT_DEFAULT;
    } else {
        __atomic_fetch_add(&num_emit_store_translations, 1, __ATOMIC_RELAXED);
        return DR_EMIT_STORE_TRANSLATIONS;
    }
}

static bool
event_restore(void *drcontext, bool restore_memory, dr_restore_state_info_t *info)
{
#ifdef X86
    /* Because we set the translation (to avoid detach timeouts) we need to restore
     * our register too.
     */
    if (info->fragment_info.cache_start_pc == NULL ||
        /* At the first instr should require no translation. */
        info->raw_mcontext->pc <= info->fragment_info.cache_start_pc)
        return true;
#    ifdef X64
    /* We have a code cache address here: verify load_from is not 32-bit-displacement
     * reachable from here.
     */
    assert(
        (ptr_int_t)load_from - (ptr_int_t)info->fragment_info.cache_start_pc < INT_MIN ||
        (ptr_int_t)load_from - (ptr_int_t)info->fragment_info.cache_start_pc > INT_MAX);
#    endif
    instr_t inst;
    instr_init(drcontext, &inst);
    byte *pc = info->fragment_info.cache_start_pc;
    while (pc < info->raw_mcontext->pc) {
        instr_reset(drcontext, &inst);
        pc = decode(drcontext, pc, &inst);
        bool tls;
        uint offs;
        bool spill;
        reg_id_t reg;
        if (instr_is_reg_spill_or_restore(drcontext, &inst, &tls, &spill, &reg, &offs) &&
            tls && !spill && reg == DR_REG_XAX) {
            /* The target point is past our restore. */
            instr_free(drcontext, &inst);
            return true;
        }
    }
    instr_free(drcontext, &inst);
    reg_set_value(DR_REG_XAX, info->mcontext, dr_read_saved_reg(drcontext, SPILL_SLOT_1));
#endif
    return true;
}

static void
event_exit(void)
{
    dr_custom_free(NULL, 0, load_from, dr_page_size());
    dr_unregister_bb_event(event_bb);
    dr_unregister_restore_state_ex_event(event_restore);
    dr_unregister_exit_event(event_exit);
}

DR_EXPORT void
dr_client_main(client_id_t id, int argc, const char *argv[])
{
    if(plain_profile)return;
    static bool do_once;
    if (!do_once) {
        fputs("detach public client initialized\n",stderr);
        do_once = true;
    }
    dr_register_bb_event(event_bb);
    dr_register_restore_state_ex_event(event_restore);
    dr_register_exit_event(event_exit);

    /* Try to get an address that is not 32-bit-displacement reachable from
     * the cache.  We have an assert on reachability in event_restore().
     */
    load_from = dr_custom_alloc(NULL, 0, dr_page_size(),
                                DR_MEMPROT_READ | DR_MEMPROT_WRITE, NULL);
}


static void wait_sem(sem_t *sem) {int r;do r=sem_wait(sem);while(r&&errno==EINTR);if(r)abort();}
static void compare(const char *name,unsigned index,uint64_t got,uint64_t expected) {
    if(got!=expected){fprintf(stderr,"detach %s[%u]: got%llx expected%llx\n",name,index,(unsigned long long)got,(unsigned long long)expected);failures++;}
}
static void *worker(void *unused) {
    (void)unused;sem_post(&prepared);wait_sem(&start);
    if(dr_app_running_under_dynamorio()!=!native_profile)abort();
    int selfmod=kind==1||kind==2||kind==4||kind==6;
    long page=sysconf(_SC_PAGESIZE);uintptr_t lo=(uintptr_t)cases[kind].run&~((uintptr_t)page-1);
    size_t len=(((uintptr_t)cases[kind].end+(uintptr_t)page-1)&~((uintptr_t)page-1))-lo;
    if(selfmod&&mprotect((void *)lo,len,PROT_READ|PROT_WRITE|PROT_EXEC))abort();
    cases[kind].run();
    if(dr_app_running_under_dynamorio())abort();
    if(selfmod&&mprotect((void *)lo,len,PROT_READ|PROT_EXEC))abort();
    return NULL;
}

/* Unexpected application faults remain failures, with only public kernel frame
 * evidence. DR's internal selfmod protection faults are not application events. */
static void fatal(int sig,siginfo_t *info,void *raw) {
    ucontext_t *u=raw;uint64_t values[5];
#if defined(__x86_64__)
    values[0]=u->uc_mcontext.gregs[REG_RIP];values[1]=u->uc_mcontext.gregs[REG_RSP];
    values[2]=u->uc_mcontext.gregs[REG_RAX];values[3]=u->uc_mcontext.gregs[REG_RDX];
#else
    values[0]=u->uc_mcontext.pc;values[1]=u->uc_mcontext.sp;
    values[2]=u->uc_mcontext.regs[0];values[3]=u->uc_mcontext.regs[2];
#endif
    values[4]=(uintptr_t)info->si_addr;
    static const char hex[]="0123456789abcdef";char line[160];unsigned n=0;
    const char *prefix="detach unexpected fault PC SP AX/X0 DX/X2 address:";
    while(*prefix)line[n++]=*prefix++;
    for(unsigned i=0;i<5;i++){line[n++]=' ';for(int j=15;j>=0;j--)line[n++]=hex[(values[i]>>(j*4))&15];}
    line[n++]='\n';ssize_t wrote=write(STDERR_FILENO,line,n);(void)wrote;_exit(128+sig);
}

int main(int argc,char **argv) {
    if(argc!=3)return 2;native_profile=!strcmp(argv[1],"native");
    plain_profile=!strcmp(argv[1],"plain");
    if(!native_profile&&!plain_profile&&strcmp(argv[1],"attach"))return 2;
    kind=-1;for(unsigned i=0;i<sizeof cases/sizeof cases[0];i++)if(!strcmp(argv[2],cases[i].name))kind=(int)i;
    if(kind<0)return 2;
    struct sigaction error={0};error.sa_sigaction=fatal;error.sa_flags=SA_SIGINFO;sigemptyset(&error.sa_mask);
    if(sigaction(SIGSEGV,&error,NULL)||sigaction(SIGBUS,&error,NULL))return 2;
    void *stack=mmap(NULL,128*1024,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);if(stack==MAP_FAILED)return 2;
    detach_safe_stack=(uintptr_t)stack+128*1024;
#if defined(__x86_64__)
    for(unsigned i=0;i<16;i++) {
        uint64_t lo=UINT64_C(0x2384626433832795)<<i,hi=UINT64_C(0x3141592653589793)<<i;
        memcpy(detach_vector_expect[i],&lo,8);memcpy(detach_vector_expect[i]+8,&hi,8);
    }
#else
    for(unsigned i=0;i<32;i++)for(unsigned j=0;j<16;j++)detach_vector_expect[i][j]=(unsigned char)(i+15*j);
#endif
    if(sem_init(&prepared,0,0)||sem_init(&start,0,0))return 2;
    pthread_t thread;if(pthread_create(&thread,NULL,worker,NULL))return 2;wait_sem(&prepared);
    if(!native_profile&&(dr_app_setup_and_start()||!dr_app_running_under_dynamorio()))abort();
    sem_post(&start);
    while(!atomic_load(&detach_progress))sched_yield();
    fprintf(stderr,"detach loop positively executed: kind%d app=%p progress%d asm-blocks%d\n",kind,(void *)cases[kind].run,atomic_load(&detach_progress),asm_blocks);
    if(!native_profile)dr_app_stop_and_cleanup();
    if(dr_app_running_under_dynamorio())abort();
    atomic_store(&detach_released,1);if(pthread_join(thread,NULL))return 2;
    if(kind<3) {
        for(unsigned i=0;i<sizeof expected_gpr/sizeof expected_gpr[0];i++) {
#if defined(__x86_64__)
            if((kind==1&&(i==0||i==2))||(kind==2&&(i==1||i==3)))continue;
#else
            if(i==0||(kind==1&&i==2)||(kind==2&&(i==5||i==6)))continue;
#endif
            compare("GPR",i,detach_gpr[i],expected_gpr[i]);
        }
#if defined(__x86_64__)
        unsigned vectors=16;
#else
        unsigned vectors=32;
#endif
        for(unsigned i=0;i<vectors;i++)for(unsigned j=0;j<16;j++)compare("vector byte",i*16+j,detach_vector_got[i][j],detach_vector_expect[i][j]);
    } else if(kind==3||kind==4) {
#if defined(__x86_64__)
        compare("RFLAGS",0,detach_status[0],0xed7);
#else
        compare("NZCV",0,detach_status[0],0xa0000000);compare("FPCR",0,detach_status[1],0x07800000);compare("FPSR",0,detach_status[2],0x9c);
#endif
    } else {
        compare("SP",0,detach_stack_observed,detach_safe_stack);
#if defined(__x86_64__)
        compare("redzone",0,detach_redzone[0],expected_gpr[0]);compare("redzone",1,detach_redzone[1],expected_gpr[2]);
#endif
    }
    if(!native_profile&&!plain_profile&&(!num_emit_default||!num_emit_store_translations||!asm_blocks))failures++;
    fprintf(stderr,"detach emit default%d stored%d\n",num_emit_default,num_emit_store_translations);
    if(munmap(stack,128*1024)||sem_destroy(&prepared)||sem_destroy(&start))return 2;
    if(failures)return 1;
    printf("OK detach context %s %s: source register vector status stack assertions preserved\n",argv[1],argv[2]);return 0;
}
