#include "dr_api.h"
static volatile int blocks;
static dr_emit_flags_t bb(void *dc, void *tag, instrlist_t *list, bool trace, bool translate) {
    (void)dc; (void)tag; (void)list; (void)trace; (void)translate;
    dr_atomic_add32_return_sum(&blocks, 1);
    return DR_EMIT_DEFAULT;
}
static void done(void) { dr_fprintf(STDERR, "PUBLIC_BB blocks=%d\n", blocks); }
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char **argv) {
    (void)id; (void)argc; (void)argv;
    dr_register_bb_event(bb);
    dr_register_exit_event(done);
}
