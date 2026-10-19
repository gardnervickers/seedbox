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

/* Source356c1821045a4b5678d0813e132074324623df6e:
 * detach_state_shared.c thread_check_sigstate/from_handler. This external
 * implementation retains real public static DR detach and exact per-thread
 * signal-mask/altstack invariants, with positive gates replacing polling sleeps.
 * Explicit SA_SIGINFO|SA_ONSTACK removes original x86 uninitialized sa_flags;
 * actual stack location and live SS_ONSTACK are checked on BOTH Linux ISAs.
 * This does not cover the source GPR/status/SP cache/DR assembly matrix. */
#define _GNU_SOURCE
#include "dr_api.h"
#include "dr_app.h"
#include "support.h"
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdatomic.h>
#include <string.h>
#include <ucontext.h>

static sem_t prepared, start;
static _Atomic int detach_ready, released, failures;
static int native_profile, handler_case;
static stack_t original_stack;
static sigset_t application_mask;
static void require(int value) {if(!value)atomic_fetch_add(&failures,1);}
static void wait_sem(sem_t *s) {int result;do result=sem_wait(s);while(result&&errno==EINTR);if(result)abort();}
static void check_mask(const sigset_t *want,const sigset_t *got) {
    for(int sig=1;sig<NSIG;sig++)require(sigismember(want,sig)==sigismember(got,sig));
}
static void query_mask(sigset_t *mask) {
    memset(mask,0,sizeof *mask);if(pthread_sigmask(SIG_BLOCK,NULL,mask))abort();
}
static void check_stack(int onstack) {
    stack_t got={0};if(sigaltstack(NULL,&got))abort();
    require(got.ss_sp==original_stack.ss_sp);
    require(got.ss_size==original_stack.ss_size);
    require(got.ss_flags==(onstack?SS_ONSTACK:0));
}
static void await_detach(void) {
    atomic_store_explicit(&detach_ready,1,memory_order_release);
    while(!atomic_load_explicit(&released,memory_order_acquire))sched_yield();
}
static void handler(int sig,siginfo_t *info,void *raw) {
    ucontext_t *frame=raw;volatile char stack_byte=0;
    require(sig==SIGUSR1&&info->si_code==SI_TKILL&&info->si_pid==getpid());
    require((uintptr_t)&stack_byte>=(uintptr_t)original_stack.ss_sp&&
            (uintptr_t)&stack_byte<(uintptr_t)original_stack.ss_sp+original_stack.ss_size);
    require(frame->uc_stack.ss_sp==original_stack.ss_sp&&
            frame->uc_stack.ss_size==original_stack.ss_size&&frame->uc_stack.ss_flags==0);
    check_mask(&application_mask,&frame->uc_sigmask);
    sigset_t active_before={0},active_after={0};query_mask(&active_before);
    require(sigismember(&active_before,SIGUSR1)==1);
    require(sigismember(&active_before,SIGUSR2)==1);
    require(sigismember(&active_before,SIGILL)==1);
    check_stack(1);await_detach();check_stack(1);
    query_mask(&active_after);check_mask(&active_before,&active_after);
    check_mask(&application_mask,&frame->uc_sigmask);
    require(frame->uc_stack.ss_sp==original_stack.ss_sp&&
            frame->uc_stack.ss_size==original_stack.ss_size&&frame->uc_stack.ss_flags==0);
}
static void *worker(void *unused) {
    (void)unused;original_stack=(stack_t){.ss_sp=malloc(SIGSTKSZ*2),.ss_size=SIGSTKSZ*2};
    if(!original_stack.ss_sp||sigaltstack(&original_stack,NULL))abort();
    sigemptyset(&application_mask);sigaddset(&application_mask,SIGUSR2);sigaddset(&application_mask,SIGILL);
    if(!handler_case)sigaddset(&application_mask,SIGUSR1);
    if(pthread_sigmask(SIG_SETMASK,&application_mask,NULL))abort();
    if(handler_case) {
        struct sigaction action={0};action.sa_sigaction=handler;action.sa_flags=SA_SIGINFO|SA_ONSTACK;
        sigfillset(&action.sa_mask);if(sigaction(SIGUSR1,&action,NULL))abort();
    }
    sem_post(&prepared);wait_sem(&start);
    require(dr_app_running_under_dynamorio()==!native_profile);
    if(handler_case) {
        if(syscall(SYS_tgkill,getpid(),gettid(),SIGUSR1))abort();
    } else {check_stack(0);await_detach();}
    require(!dr_app_running_under_dynamorio());
    sigset_t got={0};query_mask(&got);check_mask(&application_mask,&got);check_stack(0);
    stack_t disable={.ss_flags=SS_DISABLE};if(sigaltstack(&disable,NULL))abort();
    free(original_stack.ss_sp);return NULL;
}
DR_EXPORT void dr_client_main(client_id_t id,int argc,const char *argv[]) {
    (void)id;(void)argc;(void)argv;
}
int main(int argc,char **argv) {
    if(argc!=3)return 2;
    native_profile=!strcmp(argv[1],"native");
    if(!native_profile&&strcmp(argv[1],"attach"))return 2;
    handler_case=!strcmp(argv[2],"handler");if(!handler_case&&strcmp(argv[2],"ordinary"))return 2;
    if(sem_init(&prepared,0,0)||sem_init(&start,0,0))return 2;
    pthread_t thread;if(pthread_create(&thread,NULL,worker,NULL))return 2;
    wait_sem(&prepared);
    if(!native_profile&&(dr_app_setup_and_start()||!dr_app_running_under_dynamorio()))abort();
    sem_post(&start);
    while(!atomic_load_explicit(&detach_ready,memory_order_acquire))sched_yield();
    if(!native_profile)dr_app_stop_and_cleanup();
    require(!dr_app_running_under_dynamorio());
    atomic_store_explicit(&released,1,memory_order_release);
    if(pthread_join(thread,NULL)||sem_destroy(&start)||sem_destroy(&prepared))return 2;
    if(atomic_load(&failures)){fprintf(stderr,"detach signal failures%d\n",atomic_load(&failures));return 1;}
    printf("OK detach signal %s %s: masks altstack live-handler frame sigreturn preserved\n",argv[1],argv[2]);return 0;
}
