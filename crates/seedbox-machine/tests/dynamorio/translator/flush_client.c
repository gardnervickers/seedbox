/* Public-API adaptation of the seven operation selectors in
 * suite/tests/api/a64_prop_flush.c, DR 356c1821. Source license retained in
 * licenses/a64_prop_flush.c.txt. This is the single-thread API-contract slice. */
#include "dr_api.h"
#include <string.h>
static app_pc target, boundary;
static byte *unused;
static const char *mode;
static uint64 entries, boundaries, builds, callbacks;
static bool issued, flushed;
static unsigned generation;
static void completed(void *data) { DR_ASSERT(data == NULL); callbacks++; flushed = true; }
static void delayed(int id) { DR_ASSERT(id == 73); callbacks++; flushed = true; }
static void entered(unsigned built_generation)
{
    entries++;
    if (flushed && strstr(mode, "unexec") == NULL)
        DR_ASSERT(built_generation == generation);
}
static void flush_once(app_pc resume)
{
    boundaries++;
    if (issued) return;
    issued = true;
    generation++;
    bool unexec = strstr(mode, "unexec") != NULL;
    app_pc begin = unexec ? unused : target;
    bool ok;
    if (!strcmp(mode, "unlink")) {
        ok = dr_unlink_flush_region(begin, 1);
        flushed = ok;
    } else if (!strcmp(mode, "delay_cb") || !strcmp(mode, "delay_unexec")) {
        ok = dr_delay_flush_region(begin, 1, 73, delayed);
    } else if (!strcmp(mode, "delay_nocb")) {
        ok = dr_delay_flush_region(begin, 1, 73, NULL);
    } else {
        /* Explicit NULL callback exercises source 3b79a8b1, including the
         * empty-region case. full_ex checks synchronous exactly-once callback. */
        ok = !strcmp(mode, "full") ? dr_flush_region(begin, 1) :
             dr_flush_region_ex(begin, 1, !strcmp(mode, "full_ex") ? completed : NULL, NULL);
        DR_ASSERT(ok);
        flushed = true;
        if (!strcmp(mode, "full_ex")) DR_ASSERT(callbacks == 1);
        dr_mcontext_t mc = {sizeof(mc), DR_MC_ALL};
        DR_ASSERT(dr_get_mcontext(dr_get_current_drcontext(), &mc));
        mc.pc = resume;
        dr_redirect_execution(&mc);
        DR_ASSERT(false);
    }
    DR_ASSERT(ok);
}
static dr_emit_flags_t block(void *dc, void *tag, instrlist_t *bb, bool trace, bool translating)
{
    (void)tag; (void)trace; (void)translating;
    instr_t *first = instrlist_first_app(bb);
    if (!first) return DR_EMIT_DEFAULT;
    app_pc pc = instr_get_app_pc(first);
    if (pc == target) {
        builds++;
        dr_insert_clean_call(dc, bb, first, (void *)entered, false, 1,
                             OPND_CREATE_INT32(generation));
    }
    if (pc == boundary)
        dr_insert_clean_call(dc, bb, first, (void *)flush_once, false, 1,
                             OPND_CREATE_INTPTR(pc));
    return DR_EMIT_DEFAULT;
}
static void finish(void)
{
    bool cb = !strcmp(mode, "full_ex") || !strcmp(mode, "delay_cb") || !strcmp(mode, "delay_unexec");
    DR_ASSERT(issued && entries == 256 && boundaries >= 256);
    DR_ASSERT(callbacks == (cb ? 1 : 0));
    if (!strstr(mode, "unexec") && strcmp(mode, "delay_nocb")) {
        DR_ASSERT(flushed && builds >= 2);
    }
    dr_fprintf(STDERR, "flush-client: mode=%s entries=%llu builds=%llu callbacks=%llu\n",
               mode, (unsigned long long)entries, (unsigned long long)builds,
               (unsigned long long)callbacks);
    dr_nonheap_free(unused, 4096);
}
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char **argv)
{
    (void)id;
    DR_ASSERT(argc == 2);
    mode = argv[1];
    module_data_t *app = dr_get_main_module();
    DR_ASSERT(app);
    target = (app_pc)dr_get_proc_address(app->handle, "translator_target");
    boundary = (app_pc)dr_get_proc_address(app->handle, "translator_boundary");
    DR_ASSERT(target && boundary);
    dr_free_module_data(app);
    unused = dr_nonheap_alloc(4096, DR_MEMPROT_READ | DR_MEMPROT_EXEC);
    DR_ASSERT(unused);
    dr_register_bb_event(block);
    dr_register_exit_event(finish);
}
