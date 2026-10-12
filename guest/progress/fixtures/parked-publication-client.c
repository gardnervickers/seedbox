/* Restartable predicate-wait client. The unchanged raw client is retained
 * separately as the negative control. No drreg state is reserved here. */
#include "dr_api.h"

#include <stdint.h>

static uint8_t *target_block;
static uint32_t *park_req, *park_ack;
static bool loaded;

static const char *target_path;

static void
park_call(void)
{
    uint32_t r;
    if (park_req == NULL)
        return;
    r = __atomic_load_n(park_req, __ATOMIC_ACQUIRE);
    if (r == 0)
        return;
    dr_mcontext_t context = {sizeof(context), DR_MC_ALL};
    if (!dr_get_mcontext(dr_get_current_drcontext(), &context))
        dr_abort();
    context.pc = target_block;
    dr_app_predicate_wait((volatile int *)park_req, r, (volatile int *)park_ack,
                          &context);
    dr_abort();
}

static uint64_t
parse_hex(const char **p)
{
    uint64_t v = 0;
    while (**p == ' ')
        (*p)++;
    for (;;) {
        char c = **p;
        uint d;
        if (c >= '0' && c <= '9')
            d = (uint)(c - '0');
        else if (c >= 'a' && c <= 'f')
            d = (uint)(c - 'a' + 10);
        else
            break;
        v = v * 16 + d;
        (*p)++;
    }
    return v;
}

static void
load_target(void *drcontext)
{
    char buf[128];
    ssize_t got;
    const char *p = buf;
    file_t f = dr_open_file(target_path, DR_FILE_READ);
    if (f == INVALID_FILE)
        return;
    got = dr_read_file(f, buf, sizeof(buf) - 1);
    dr_close_file(f);
    if (got <= 0)
        return;
    buf[got] = 0;
    target_block = (uint8_t *)(uintptr_t)parse_hex(&p);
    park_req = (uint32_t *)(uintptr_t)parse_hex(&p);
    park_ack = (uint32_t *)(uintptr_t)parse_hex(&p);
    if (target_block == NULL || park_req == NULL || park_ack == NULL) {
        dr_fprintf(STDERR, "parked predicate: bad target file: %s\n", buf);
        dr_abort();
    }
    loaded = true;
    dr_fprintf(STDERR, "parked predicate: target=%p req=%p ack=%p\n", target_block, park_req,
               park_ack);
}

static dr_emit_flags_t
bb_event(void *drcontext, void *tag, instrlist_t *bb, bool for_trace, bool translating)
{
    (void)for_trace;
    (void)translating;
    if (!loaded)
        load_target(drcontext);
    if (loaded && tag == (void *)target_block) {
        dr_insert_clean_call_ex(drcontext, bb, instrlist_first(bb), (void *)park_call,
                                DR_CLEANCALL_READS_APP_CONTEXT |
                                    DR_CLEANCALL_MULTIPATH, 0);
    }
    return DR_EMIT_DEFAULT;
}

DR_EXPORT void
dr_client_main(client_id_t id, int argc, const char *argv[])
{
    (void)id;
    if (argc != 2)
        dr_abort();
    target_path = argv[1];
    dr_register_bb_event(bb_event);
}
