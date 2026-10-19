#include "dr_api.h"
#include <string.h>

static bool store;
static dr_emit_flags_t
basic_block(void *context, void *tag, instrlist_t *block, bool trace, bool translating)
{
    instr_t *first = instrlist_first_app(block);
    (void)tag;
    (void)trace;
    if (first != NULL && instr_raw_bits_valid(first)) {
        byte *raw = instr_get_raw_bits(first);
        bool target = false;
#ifdef X86_64
        target = instr_length(context, first) == 2 && raw[0] == 0xff &&
                 (raw[1] == 0x17 || raw[1] == 0x27);
#elif defined(AARCH64)
        target = instr_length(context, first) == 4 && raw[0] == 0x10 &&
                 raw[1] == 0x00 && raw[2] == 0x40 && raw[3] == 0xf9;
#endif
        if (target)
            dr_fprintf(STDERR, "fault-context-client: store=%d translating=%d\n",
                       store, translating);
    }
    return store ? DR_EMIT_STORE_TRANSLATIONS : DR_EMIT_DEFAULT;
}

DR_EXPORT void
dr_client_main(client_id_t id, int argc, const char *argv[])
{
    (void)id;
    store = argc == 2 && !strcmp(argv[1], "store");
    DR_ASSERT(argc == 1 || store);
    dr_register_bb_event(basic_block);
}
