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

/* Source 356c1821045a4b5678d0813e132074324623df6e a64_prop_lifecycle.c; external application model and public static DR lifecycle. */
#define _GNU_SOURCE
#include "dr_api.h"
#include "dr_app.h"
#include "support.h"
#include "state_model.h"
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <string.h>
#include <sys/mman.h>
#define PSTATE_BTYPE 0xc00ULL
#define PSTATE_SSBS 0x1000ULL
#define PSTATE_DIT 0x1000000ULL
#define PSIG (SIGRTMIN+3)
static int g_no_faults,g_native_profile,g_variant;
static volatile int debug_option_confirmed;
/***************************************************************************
 * Controller state shared with the workers.
 */
/* Bumped before and after every transition: odd while one is in progress.
 * A run that sees a different value at its end than at its start was
 * interrupted by a transition.
 */
static volatile uint64_t g_transitions;
/* Phase: 0 native, 1 under DR (set after each transition completes). */
static volatile int g_phase;
static volatile int g_stop;
static pthread_mutex_t out_lock = PTHREAD_MUTEX_INITIALIZER;
/* Where comparisons that are only counted print their differences. */
static FILE *g_devnull;

/***************************************************************************
 * Block workers.
 */
#define MAXWK 8
#define NBLK 3

typedef struct {
    int id;
    pthread_t th;
    a64p_worker_t w;
    uint64_t seed;
    block_t *b[NBLK];
    result_t *expect[NBLK];
    int nblk;
    volatile int ready;
    /* Counts. */
    volatile long runs, fails, straddled, phase_runs[2];
    long masked_dit;
} worker_t;

static worker_t g_workers[MAXWK];
static int g_nworkers;

static void
on_masked(int i, fault_rec_t *n, fault_rec_t *d)
{
    /* Known deviations (BTYPE: DR-built frames; SSBS: kernel nondeterminism,
     * seen natively too) are counted by the caller's comparison result only.
     */
    (void)i;
    (void)n;
    (void)d;
}

static void
report_mismatch(worker_t *wk, int bi, result_t *got, uint64_t t0, uint64_t t1)
{
    pthread_mutex_lock(&out_lock);
    fprintf(stderr,
            "FAIL [lifecycle] worker %d block %d (seed %llu) run %ld: state differs from "
            "the native run (transitions %llu -> %llu, phase %d)\n",
            wk->id, bi, (unsigned long long)wk->b[bi]->seed, wk->runs,
            (unsigned long long)t0, (unsigned long long)t1, g_phase);
    a64p_cmp_results(stderr, wk->b[bi], wk->expect[bi], got);
    pthread_mutex_unlock(&out_lock);
}

/* Long blocks (an outer loop) without faults, or blocks with planned faults
 * and BRKs (whose count an outer loop would multiply).
 */
static void
lc_opts(gen_opts_t *o, int long_block)
{
    memset(o, 0, sizeof(*o));
    o->allow_excl = 1;
    o->allow_svc = 1;
    o->allow_fpcr = 1;
    o->allow_ctr = 1;
    o->outer_loop = long_block;
    o->allow_faults = o->allow_brk = !long_block;
    o->len = 60;
    o->dyn_max = 20000;
}

/* Blocks do not change PSTATE.DIT: a frame DR builds keeps only NZCV of PSTATE
 * (review R3, checked by a64_prop_signal), so a block that set DIT and faulted
 * while DR attached or detached would read DIT clear afterwards, in registers
 * and in memory.
 */
static void
lc_init_features(void)
{
    a64p_init_features();
    feat.dit = 0;
}

static void *
block_worker(void *arg)
{
    worker_t *wk = (worker_t *)arg;
    a64p_worker_bind(&wk->w);
    a64p_setup_maps();
    a64p_install_handlers(&wk->w, 0);
    /* The process signal goes to the other threads: a handler frame here could
     * land in the compared stack.
     */
    sigset_t psig_set;
    sigemptyset(&psig_set);
    sigaddset(&psig_set, PSIG);
    pthread_sigmask(SIG_BLOCK, &psig_set, NULL);
    gen_opts_t o;
    result_t *tmp = malloc(sizeof(*tmp));
    /* Native expectations, computed before DR is ever attached: blocks whose
     * two native runs differ, or that fault unplannedly, are not used.
     */
    for (int tries = 0; wk->nblk < NBLK && tries < 8 * NBLK; tries++) {
        block_t *b = malloc(sizeof(*b));
        result_t *e = malloc(sizeof(*e));
        uint64_t s = wk->seed * 1000 + (uint64_t)tries;
        lc_opts(&o, (wk->nblk % 2) == 0 || g_no_faults);
        if (!a64p_make_block(b, &o, s)) {
            free(b);
            free(e);
            continue;
        }
        a64p_install_code(&wk->w, b, 0);
        a64p_run_block(&wk->w, b, e);
        a64p_run_block(&wk->w, b, tmp);
        if (!a64p_check_planned(b, e) || a64p_cmp_results(g_devnull, b, e, tmp) != 0) {
            free(b);
            free(e);
            continue;
        }
        wk->b[wk->nblk] = b;
        wk->expect[wk->nblk] = e;
        wk->nblk++;
    }
    __atomic_store_n(&wk->ready, 1, __ATOMIC_RELEASE);
    rng_t r = { wk->seed ^ 0x5bd1e995ULL };
    int cur = -1;
    while (!__atomic_load_n(&g_stop, __ATOMIC_ACQUIRE) && wk->nblk > 0) {
        int bi = (int)rn(&r, (uint32_t)wk->nblk);
        if (bi != cur) {
            /* Reinstalling code goes through munmap/mmap: also under DR. */
            a64p_install_code(&wk->w, wk->b[bi], 0);
            cur = bi;
        }
        uint64_t t0 = __atomic_load_n(&g_transitions, __ATOMIC_ACQUIRE);
        int ph = __atomic_load_n(&g_phase, __ATOMIC_ACQUIRE);
        bool under=dr_app_running_under_dynamorio();
        a64p_run_block(&wk->w, wk->b[bi], tmp);
        uint64_t t1 = __atomic_load_n(&g_transitions, __ATOMIC_ACQUIRE);
        __atomic_add_fetch(&wk->runs,1,__ATOMIC_RELEASE);
        int straddled = t0 != t1 || (t0 & 1) != 0;
        if (straddled) {
            wk->straddled++;
        } else {
            __atomic_add_fetch(&wk->phase_runs[ph],1,__ATOMIC_RELEASE);
            if(under!=(ph==1&&!g_native_profile))wk->fails++;
        }
        if (a64p_cmp_results(g_devnull, wk->b[bi], wk->expect[bi], tmp) != 0) {
            wk->fails++;
            if (wk->fails <= 3)
                report_mismatch(wk, bi, tmp, t0, t1);
        }
    }
    free(tmp);
    return NULL;
}

/* The source controller's timed on/off intervals become positive work in EACH
 * of its original60 rounds: every worker completes8 full model checks in each
 * phase. This retains and strengthens original50/worker total progress floor.
 * The source's process-signal maximum is retained as a finite acknowledged
 * population, so no arbitrary elapsed duration limits signal coverage. */
static int pipes[2];
static sem_t received,pipe_ready,thread_signal_done;
static volatile int pipe_tid;
static volatile long operations,handler_runs,handler_bad,process_sent;
#define PROCESS_SIGNALS (1<<17)
static volatile unsigned process_got[PROCESS_SIGNALS];
static volatile int process_bad;
static volatile int emit_default,emit_stored,client_probes;
static app_pc probe_pc;
__attribute__((noinline,visibility("default"))) void unix_lifecycle_probe(void) {asm volatile("":::"memory");}
static void probe(void) {dr_atomic_add32_return_sum((volatile int *)&client_probes,1);}
static dr_emit_flags_t event_block(void *dc,void *tag,instrlist_t *bb,bool trace,bool translating) {
    (void)trace;(void)translating;instr_t *first=instrlist_first_app(bb);
    if(first && instr_get_app_pc(first)==probe_pc)dr_insert_clean_call(dc,bb,first,(void *)probe,false,0);
    ptr_uint_t bit=(ptr_uint_t)tag>>2; /* source A64 instruction-alignment correction */
    if(bit&1){dr_atomic_add32_return_sum((volatile int *)&emit_stored,1);return DR_EMIT_STORE_TRANSLATIONS;}
    dr_atomic_add32_return_sum((volatile int *)&emit_default,1);return DR_EMIT_DEFAULT;
}
static void client_exit(void) {dr_unregister_bb_event(event_block);dr_unregister_exit_event(client_exit);}
DR_EXPORT void dr_client_main(client_id_t id,int argc,const char *argv[]) {
    (void)id;(void)argc;(void)argv;
    if(g_variant>=3) {
        uint64 value=UINT64_MAX;
        const char *option=g_variant==3?"ldstex2cas":"steal_reg";
        uint64 expected=g_variant==3?0:27;
        if(!dr_get_integer_option(option,&value)||value!=expected) {
            fprintf(stderr,"lifecycle debug option %s not active: value%llu expected%llu\n",option,(unsigned long long)value,(unsigned long long)expected);_exit(2);
        }
        dr_atomic_add32_return_sum(&debug_option_confirmed,1);
    }
    module_data_t *app=dr_get_main_module();
    DR_ASSERT(app!=NULL);probe_pc=(app_pc)dr_get_proc_address(app->handle,"unix_lifecycle_probe");
    dr_free_module_data(app);DR_ASSERT(probe_pc!=NULL);
    dr_register_bb_event(event_block);dr_register_exit_event(client_exit);
}
static void wait_sem(sem_t *sem) {int result;do result=sem_wait(sem);while(result&&errno==EINTR);if(result)abort();}
static void *reader(void *unused) {
    (void)unused;pipe_tid=gettid();sem_post(&pipe_ready);
    for(unsigned want=1;;want++) {
        unsigned value=0;ssize_t result;do result=read(pipes[0],&value,sizeof value);while(result<0&&errno==EINTR);
        if(result!=sizeof value)abort();if(!value)return NULL;if(value!=want)abort();
        __atomic_add_fetch(&operations,1,__ATOMIC_RELEASE);sem_post(&received);
    }
}
static void send_operation(unsigned operation) {
    ssize_t result;do result=write(pipes[1],&operation,sizeof operation);while(result<0&&errno==EINTR);
    if(result!=sizeof operation)abort();if(operation)wait_sem(&received);
}
static __thread volatile uint64_t signal_checksum;
static void checksum_handler(int sig) {
    if(sig!=SIGUSR2)_exit(3);uint64_t sum=0;
    for(volatile int i=1;i<=20000;i++)sum+=(uint64_t)i*i;
    signal_checksum=sum;sem_post(&thread_signal_done);
}
static void *signal_worker(void *unused) {
    (void)unused;pid_t tid=gettid();
    while(!__atomic_load_n(&g_stop,__ATOMIC_ACQUIRE)) {
        signal_checksum=0;if(syscall(SYS_tgkill,getpid(),tid,SIGUSR2))abort();
        wait_sem(&thread_signal_done);
        if(signal_checksum!=UINT64_C(2666866670000))__atomic_add_fetch(&handler_bad,1,__ATOMIC_RELEASE);
        __atomic_add_fetch(&handler_runs,1,__ATOMIC_RELEASE);
    }
    return NULL;
}
static void process_handler(int sig,siginfo_t *info,void *context) {
    (void)context;int value=info->si_value.sival_int;
    if(sig!=PSIG||info->si_code!=SI_QUEUE||info->si_pid!=getpid()||value<0||value>=PROCESS_SIGNALS)
        __atomic_add_fetch(&process_bad,1,__ATOMIC_RELAXED);
    else __atomic_add_fetch(&process_got[value],1,__ATOMIC_RELEASE);
}
static void *process_worker(void *unused) {
    (void)unused;
    for(int i=0;i<PROCESS_SIGNALS;i++) {
        union sigval value={.sival_int=i};if(sigqueue(getpid(),PSIG,value))abort();
        __atomic_store_n(&process_sent,i+1,__ATOMIC_RELEASE);
        while(!__atomic_load_n(&process_got[i],__ATOMIC_ACQUIRE))sched_yield();
    }
    return NULL;
}
static void phase_progress(int phase) {
    long before[MAXWK];
    for(int i=0;i<g_nworkers;i++)before[i]=__atomic_load_n(&g_workers[i].phase_runs[phase],__ATOMIC_ACQUIRE);
    for(int i=0;i<g_nworkers;i++) {
        while(__atomic_load_n(&g_workers[i].phase_runs[phase],__ATOMIC_ACQUIRE)<before[i]+8)sched_yield();
    }
}
static void transition(int attach) {
    __atomic_add_fetch(&g_transitions,1,__ATOMIC_ACQ_REL);
    if(!g_native_profile) {
        if(attach) {
            if(dr_app_setup_and_start()||!dr_app_running_under_dynamorio())abort();
            unix_lifecycle_probe();
        } else {dr_app_stop_and_cleanup();if(dr_app_running_under_dynamorio())abort();}
    }
    __atomic_store_n(&g_phase,attach,__ATOMIC_RELEASE);
    __atomic_add_fetch(&g_transitions,1,__ATOMIC_ACQ_REL);
}
int main(int argc,char **argv) {
    if(argc!=2)return 2;
    int variant=!strcmp(argv[1],"native")?0:!strcmp(argv[1],"default")?1:!strcmp(argv[1],"notraces")?2:!strcmp(argv[1],"nocas")?3:!strcmp(argv[1],"steal27")?4:-1;
    if(variant<0)return 2;g_native_profile=variant==0;g_variant=variant;
#ifndef UNIX_DEBUG_CORE
    if(variant>=3){fputs("lifecycle nocas/steal27 require actual matching Debug core fixture\n",stderr);return 2;}
#endif
    if(variant>=3&&setenv("DYNAMORIO_OPTIONS",variant==3?"-no_ldstex2cas":"-steal_reg 27",1))return 2;
    if(variant==2&&setenv("DYNAMORIO_OPTIONS","-disable_traces",1))return 2;
    /* Original generator uses a4KiB guard/data/stack layout. Native64KiB
     * adaptation is still a separate oracle/layout requirement, never success. */
    if(sysconf(_SC_PAGESIZE)!=4096){fputs("lifecycle model requires original4KiB application guard layout\n",stderr);return 2;}
    lc_init_features();g_devnull=fopen("/dev/null","w");if(!g_devnull)return 2;
    /* Exact source named kernel nondeterministic bits; no added register mask.
     * Source excludes DIT-changing blocks (lc_init_features); signal_model.c
     * retains its separate explicit DIT/signal oracle on supporting hardware. */
    a64p_fault_pstate_ignore=PSTATE_BTYPE|PSTATE_SSBS;a64p_on_masked_pstate=on_masked;
    sem_init(&received,0,0);sem_init(&pipe_ready,0,0);sem_init(&thread_signal_done,0,0);
    struct sigaction action={0};action.sa_handler=checksum_handler;action.sa_flags=SA_RESTART;sigemptyset(&action.sa_mask);
    if(sigaction(SIGUSR2,&action,NULL))return 2;
    action.sa_sigaction=process_handler;action.sa_flags=SA_SIGINFO|SA_RESTART;
    if(sigaction(PSIG,&action,NULL)||pipe(pipes))return 2;
    uint64_t seed=1+(unsigned)variant;rng_t rng={seed};g_nworkers=2+(int)rn(&rng,MAXWK-1);
    for(int i=0;i<g_nworkers;i++) {
        worker_t *w=&g_workers[i];w->id=i;w->seed=seed*64+(unsigned)i;
        a64p_worker_init_slot(&w->w,i+1);
        if(pthread_create(&w->th,NULL,block_worker,w))return 2;
    }
    for(int i=0;i<g_nworkers;i++) {
        while(!__atomic_load_n(&g_workers[i].ready,__ATOMIC_ACQUIRE))sched_yield();
        if(g_workers[i].nblk!=NBLK){fprintf(stderr,"worker%d generated%d expected3 native-admitted blocks\n",i,g_workers[i].nblk);return 1;}
    }
    pthread_t reader_thread,signal_thread,process_thread;
    if(pthread_create(&reader_thread,NULL,reader,NULL)||pthread_create(&signal_thread,NULL,signal_worker,NULL)||pthread_create(&process_thread,NULL,process_worker,NULL))return 2;
    wait_sem(&pipe_ready);unsigned operation=1;
    for(int round=0;round<60;round++) {
        send_operation(operation++);unix_wait_syscall(getpid(),pipe_tid,SYS_read);
        transition(1);send_operation(operation++);phase_progress(1);
        send_operation(operation++);unix_wait_syscall(getpid(),pipe_tid,SYS_read);
        transition(0);send_operation(operation++);phase_progress(0);
    }
    /* Actual positive completion, not a sleep/drain guess. Every source-sized
     * process signal is acknowledged exactly once, including across transitions.
     * Original attach-time lost-signal exceptions are not imported as success. */
    if(pthread_join(process_thread,NULL))return 2;
    while(__atomic_load_n(&handler_runs,__ATOMIC_ACQUIRE)<10)sched_yield();
    phase_progress(0);__atomic_store_n(&g_stop,1,__ATOMIC_RELEASE);
    if(pthread_join(signal_thread,NULL))return 2;
    for(int i=0;i<g_nworkers;i++)if(pthread_join(g_workers[i].th,NULL))return 2;
    send_operation(0);if(pthread_join(reader_thread,NULL))return 2;
    long runs=0,straddled=0;int failed=process_bad||handler_bad||process_sent!=PROCESS_SIGNALS||operations!=240;
    for(int i=0;i<PROCESS_SIGNALS;i++)failed|=process_got[i]!=1;
    for(int i=0;i<g_nworkers;i++) {
        worker_t *w=&g_workers[i];runs+=w->runs;straddled+=w->straddled;
        failed|=w->fails!=0||w->runs<50||w->phase_runs[0]<480||w->phase_runs[1]<480;
        fprintf(stderr,"model worker%d: blocks%d checked%ld phases%ld/%ld straddled%ld failures%ld\n",i,w->nblk,w->runs,w->phase_runs[0],w->phase_runs[1],w->straddled,w->fails);
    }
    if(!g_native_profile)failed|=straddled<1||emit_default<1||emit_stored<1||client_probes!=60;
    fprintf(stderr,"model: runs%ld straddled%ld handlers%ld process%ld emit%d/%d probes%d\n",runs,straddled,handler_runs,process_sent,emit_default,emit_stored,client_probes);
    if(close(pipes[0])||close(pipes[1])||fclose(g_devnull)||sem_destroy(&received)||sem_destroy(&pipe_ready)||sem_destroy(&thread_signal_done))return 2;
    if(g_variant>=3) {
        fprintf(stderr,"profile debug confirmed%d\n",debug_option_confirmed);
        failed|=debug_option_confirmed!=60;
    }
    if(failed)return 1;
    printf("OK lifecycle model %s: rounds60 blocks3/worker workers%d operations240 process131072 full state/fault comparisons\n",argv[1],g_nworkers);
    return 0;
}
