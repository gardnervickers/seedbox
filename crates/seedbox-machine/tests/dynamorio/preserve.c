/* Only the public SDK: deliberately overwrite a live application register and
 * flags at every instruction in the exported assembly probes. */
#include "dr_api.h"
#include "drmgr.h"
#include "drreg.h"

static app_pc begin, end;
static drvector_t allowed;
static int executed;

static void count(void) { ++executed; } /* These fixtures have one thread. */

static dr_emit_flags_t
insert(void *dc, void *tag, instrlist_t *bb, instr_t *where,
       bool for_trace, bool translating, void *user_data)
{
    app_pc pc = instr_get_app_pc(where);
    reg_id_t scratch;
    if (pc == NULL || pc < begin || pc >= end)
        return DR_EMIT_DEFAULT;
    dr_insert_clean_call(dc, bb, where, (void *)count, false, 0);
    DR_ASSERT(drreg_reserve_register(dc, bb, where, &allowed, &scratch) == DRREG_SUCCESS);
    DR_ASSERT(drreg_reserve_aflags(dc, bb, where) == DRREG_SUCCESS);
    instrlist_meta_preinsert(bb, where,
        XINST_CREATE_load_int(dc, opnd_create_reg(scratch), OPND_CREATE_INT32(0)));
    instrlist_meta_preinsert(bb, where,
        XINST_CREATE_cmp(dc, opnd_create_reg(scratch), OPND_CREATE_INT8(0)));
    DR_ASSERT(drreg_unreserve_aflags(dc, bb, where) == DRREG_SUCCESS);
    DR_ASSERT(drreg_unreserve_register(dc, bb, where, scratch) == DRREG_SUCCESS);
    return DR_EMIT_DEFAULT;
}

static void finish(void)
{
    dr_fprintf(STDERR, "dr-client: executed=%d\n", executed);
    DR_ASSERT(drmgr_unregister_bb_insertion_event(insert));
    drvector_delete(&allowed);
    DR_ASSERT(drreg_exit() == DRREG_SUCCESS);
    drmgr_exit();
}

DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[])
{
    drreg_options_t options = { sizeof(options), 3, true };
    module_data_t *app = dr_get_main_module();
    begin = (app_pc)dr_get_proc_address(app->handle, "probe_begin");
    end = (app_pc)dr_get_proc_address(app->handle, "probe_end");
    dr_free_module_data(app);
    DR_ASSERT(begin != NULL && end > begin);
    DR_ASSERT(drmgr_init());
    DR_ASSERT(drreg_init(&options) == DRREG_SUCCESS);
    DR_ASSERT(drreg_init_and_fill_vector(&allowed, false) == DRREG_SUCCESS);
#ifdef X86_64
    DR_ASSERT(drreg_set_vector_entry(&allowed, DR_REG_RAX, true) == DRREG_SUCCESS);
#else
    DR_ASSERT(drreg_set_vector_entry(&allowed, DR_REG_X0, true) == DRREG_SUCCESS);
#endif
    DR_ASSERT(drmgr_register_bb_instrumentation_event(NULL, insert, NULL));
    dr_register_exit_event(finish);
}
