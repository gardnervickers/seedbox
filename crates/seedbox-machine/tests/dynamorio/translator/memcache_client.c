/* Public dr_query_memory_ex oracle at explicitly acknowledged app boundaries. */
#include "dr_api.h"
static app_pc marker;
static unsigned queries;
static void query(app_pc address)
{
    dr_mem_info_t info;
    DR_ASSERT(dr_query_memory_ex(address, &info));
    DR_ASSERT(info.type == DR_MEMTYPE_FREE);
    DR_ASSERT(info.prot == DR_MEMPROT_NONE);
    queries++;
}
static dr_emit_flags_t block(void *dc, void *tag, instrlist_t *bb, bool trace, bool translating)
{
    (void)tag; (void)trace; (void)translating;
    instr_t *first = instrlist_first_app(bb);
    if (first && instr_get_app_pc(first) == marker) {
#ifdef AARCH64
        opnd_t argument = opnd_create_reg(DR_REG_X0);
#else
        opnd_t argument = opnd_create_reg(DR_REG_RDI);
#endif
        dr_insert_clean_call(dc, bb, first, (void *)query, false, 1, argument);
    }
    return DR_EMIT_DEFAULT;
}
static void finish(void)
{
    dr_fprintf(STDERR, "memcache-client: unmapped queries=%u\n", queries);
    DR_ASSERT(queries == 10);
}
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char **argv)
{
    (void)id; (void)argc; (void)argv;
    module_data_t *app = dr_get_main_module();
    marker = (app_pc)dr_get_proc_address(app->handle, "translator_memory_hole");
    DR_ASSERT(marker);
    dr_free_module_data(app);
    dr_register_bb_event(block);
    dr_register_exit_event(finish);
}
