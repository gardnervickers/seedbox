/* Diagnostic client only: distinguish actual emitted blocks from translation
 * reconstruction and observe deletion. Never edits code or publication policy. */
#include "dr_api.h"
#include "drmgr.h"
static app_pc base;
static size_t length;
static uint phase;
static bool interested(void *tag)
{
    return base != NULL && (app_pc)tag >= base && (app_pc)tag - base < length;
}
static dr_emit_flags_t observe(void *context, void *tag, instrlist_t *block,
                               bool trace, bool translating)
{
    if (interested(tag))
        dr_fprintf(STDERR, "IC_BLOCK phase=%u offset=%llu reconstruct=%d trace=%d\n",
                   phase, (uint64)((app_pc)tag - base), translating, trace);
    return DR_EMIT_DEFAULT;
}
static void deleted(void *context, void *tag)
{
    if (interested(tag))
        dr_fprintf(STDERR, "IC_DELETE phase=%u offset=%llu\n",
                   phase, (uint64)((app_pc)tag - base));
}
static bool filter(void *context, int number) { return number == 172; }
static bool syscall_before(void *context, int number)
{
    if (number == 172 && dr_syscall_get_param(context, 0) == 0x53494344UL) {
        base = (app_pc)dr_syscall_get_param(context, 1);
        length = (size_t)dr_syscall_get_param(context, 2);
        phase = (uint)dr_syscall_get_param(context, 3);
        dr_fprintf(STDERR, "IC_PHASE %u\n", phase);
    }
    return true;
}
static void finished(void) { drmgr_exit(); }
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[])
{
    DR_ASSERT(drmgr_init());
    DR_ASSERT(drmgr_register_bb_app2app_event(observe, NULL));
    DR_ASSERT(drmgr_register_filter_syscall_event(filter));
    DR_ASSERT(drmgr_register_pre_syscall_event(syscall_before));
    dr_register_delete_event(deleted);
    drmgr_register_exit_event(finished);
}
