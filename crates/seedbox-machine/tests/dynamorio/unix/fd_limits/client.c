#include "dr_api.h"
static unsigned long blocks;
static dr_emit_flags_t
block(void *context, void *tag, instrlist_t *instructions, bool trace, bool translating)
{
    (void)context; (void)tag; (void)instructions; (void)trace; (void)translating;
    ++blocks;
    return DR_EMIT_DEFAULT;
}
static void done(void)
{
    dr_fprintf(STDERR, "fd-client: pid=%d blocks=%lu\n", dr_get_process_id(), blocks);
}
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char **argv)
{
    (void)id; (void)argc; (void)argv;
    dr_fprintf(STDERR, "fd-client: init=%d\n", dr_get_process_id());
    dr_register_bb_event(block);
    dr_register_exit_event(done);
}
