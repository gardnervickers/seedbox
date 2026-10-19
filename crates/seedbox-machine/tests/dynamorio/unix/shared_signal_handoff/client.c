#include "dr_api.h"
#include "drmgr.h"
static app_pc loop_pc, marker_pc;
static unsigned requested, completed;
static void done(int id) { dr_atomic_add32_return_sum((volatile int *)&completed,1); }
static void request(void) {
    unsigned id=++requested;
    DR_ASSERT(dr_delay_flush_region(loop_pc,1,id,done));
}
static dr_emit_flags_t block(void *dc, void *tag, instrlist_t *bb, instr_t *where,
                            bool trace, bool translating, void *data) {
    if (instr_get_app_pc(where)==marker_pc)
        dr_insert_clean_call(dc,bb,where,(void*)request,false,0);
    return DR_EMIT_DEFAULT;
}
static void finish(void) {
    dr_fprintf(STDERR,"handoff-client: requested=%u completed=%u\n",requested,completed);
    DR_ASSERT(requested==64 && completed==64);
    drmgr_exit();
}
DR_EXPORT void dr_client_main(client_id_t id,int argc,const char *argv[]) {
    DR_ASSERT(drmgr_init());
    module_data_t *main=dr_get_main_module();
    loop_pc=(app_pc)dr_get_proc_address(main->handle,"shared_loop");
    marker_pc=(app_pc)dr_get_proc_address(main->handle,"flush_marker");
    DR_ASSERT(loop_pc && marker_pc);
    dr_free_module_data(main);
    DR_ASSERT(drmgr_register_bb_instrumentation_event(NULL,block,NULL));
    dr_register_exit_event(finish);
}
