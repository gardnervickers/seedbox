#include "dr_api.h"
#include <sys/time.h>
#include <sys/syscall.h>
#include <errno.h>
static int ticks[3], pre_count, post_count, ready_count, skipped, updates;
static app_pc ready_pc;
static int finished;
static void real_cb(void *dc,dr_mcontext_t *mc) { (void)dc;(void)mc;if(dr_atomic_add32_return_sum(&finished,0)) dr_abort_with_code(3);dr_atomic_add32_return_sum(&ticks[0],1); }
static void virt_cb(void *dc,dr_mcontext_t *mc) { (void)dc;(void)mc;if(dr_atomic_add32_return_sum(&finished,0)) dr_abort_with_code(3);dr_atomic_add32_return_sum(&ticks[1],1); }
static void prof_cb(void *dc,dr_mcontext_t *mc) { (void)dc;(void)mc;if(dr_atomic_add32_return_sum(&finished,0)) dr_abort_with_code(3);dr_atomic_add32_return_sum(&ticks[2],1); }
static void ready(void) {
    while(dr_atomic_add32_return_sum(&ticks[0],0)<2 ||
          dr_atomic_add32_return_sum(&ticks[1],0)<2 ||
          dr_atomic_add32_return_sum(&ticks[2],0)<2) {
        /* Finite positive-event wait; external timeout contains failure.
         * CPU work also advances CPU timers; no sleep or elapsed-time oracle. */
        for(volatile int i=0;i<10000;i++) {}
    }
    dr_atomic_add32_return_sum(&ready_count,1);
}
static bool filter(void *dc,int n) { (void)dc;return n==SYS_setitimer || n==SYS_getitimer
#ifdef SYS_alarm
    || n==SYS_alarm
#endif
    ; }
static bool pre(void *dc,int n) {
    if(!filter(dc,n)) return true;
    int count=dr_atomic_add32_return_sum(&pre_count,1);
    if(n==SYS_setitimer && dr_syscall_get_param(dc,0)==77) {
        dr_syscall_set_result(dc,(reg_t)-(ptr_int_t)EINVAL);
        dr_atomic_add32_return_sum(&skipped,1);
        return false;
    }
    if(count%31==0) {
        if(!dr_set_itimer(ITIMER_VIRTUAL,4,virt_cb) || dr_get_itimer(ITIMER_VIRTUAL)!=4) dr_abort_with_code(1);
        dr_atomic_add32_return_sum(&updates,1);
    }
    return true;
}
static void post(void *dc,int n) { if(filter(dc,n)) dr_atomic_add32_return_sum(&post_count,1); }
static dr_emit_flags_t bb(void *dc,void *tag,instrlist_t *bb,bool trace,bool translating) {
    (void)trace;(void)translating;
    for(instr_t *i=instrlist_first_app(bb); i!=NULL; i=instr_get_next_app(i))
        if(instr_get_app_pc(i)==ready_pc) dr_insert_clean_call(dc,bb,i,(void *)ready,false,0);
    return DR_EMIT_DEFAULT;
}
static void finish(void) {
    dr_atomic_add32_return_sum(&finished,1);
    dr_fprintf(STDERR,"timer-client callbacks=%d,%d,%d pre=%d post=%d ready=%d skipped=%d updates=%d\n",ticks[0],ticks[1],ticks[2],pre_count,post_count,ready_count,skipped,updates);
    if(ticks[0]<2 || ticks[1]<2 || ticks[2]<2 || pre_count!=post_count+skipped || skipped!=1 || updates==0 || ready_count!=1) dr_abort_with_code(1);
}
DR_EXPORT void dr_client_main(client_id_t id,int argc,const char *argv[]) {
    (void)id;(void)argc;(void)argv;
    module_data_t *main=dr_get_main_module();
    ready_pc=(app_pc)dr_get_proc_address(main->handle,"timer_callbacks_ready");
    dr_free_module_data(main);
    if(ready_pc==NULL) dr_abort_with_code(2);
    dr_register_filter_syscall_event(filter);dr_register_pre_syscall_event(pre);dr_register_post_syscall_event(post);
    dr_register_bb_event(bb);dr_register_exit_event(finish);
    if(!dr_set_itimer(ITIMER_REAL,3,real_cb) || !dr_set_itimer(ITIMER_VIRTUAL,4,virt_cb) || !dr_set_itimer(ITIMER_PROF,5,prof_cb)) dr_abort_with_code(2);
}
