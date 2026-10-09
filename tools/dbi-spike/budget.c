/* Experimental x86-64 block budgets. Optional raw getpid calls charge PV time.
 * Per-thread counters avoid races; they do not impose deterministic scheduling.
 * Checkpoints are before the block whose entry consumes the last budget unit.
 */
#include "dr_api.h"
#include "drmgr.h"
#include "drreg.h"
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#define BUDGET 10000
typedef struct {
    int remaining;
    uint64_t checkpoints;
    uint64_t digest;
    uint64_t anonymous_checkpoints;
    reg_t *raw_remaining;
} state_t;
static int tls;
static bool progress;
static bool fast;
static reg_id_t raw_segment;
static uint raw_offset;

static bool option_is(const char *arg, const char *want) {
    while (*want && *want == *arg) { ++want; ++arg; }
    return *want == '\0' && *arg == '\0';
}

static void charge_progress(void) {
    /* Linux x86-64 getpid. Bypass libc caching and preserve application errno.
     * DynamoRIO's clean-call boundary preserves the application register state;
     * these constraints also describe SYSCALL's clobbers to the C compiler.
     * No sched_yield: the PV kernel's syscall budget decides when to exit.
     */
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(39L)
                     : "rcx", "r11", "memory", "cc");
    DR_ASSERT(result > 0);
}

static void checkpoint(app_pc pc, ptr_uint_t offset, int anonymous) {
    void *ctx = dr_get_current_drcontext();
    state_t *s = drmgr_get_tls_field(ctx, tls);
    s->remaining = BUDGET;
    s->checkpoints++;
    s->anonymous_checkpoints += anonymous;
    /* Static image offsets are ASLR independent. Anonymous/JIT PCs deliberately
     * have no stable identity here; their offsets are zero, not a replay proof. */
    s->digest = (s->digest ^ offset) * UINT64_C(1099511628211);
    if (progress)
        charge_progress();
    if (s->checkpoints <= 4)
        dr_fprintf(STDERR, "point: %llu offset=%llx anonymous=%d\n",
                   (unsigned long long)s->checkpoints,
                   (unsigned long long)offset, anonymous);
    (void)pc;
}
/* No module lookup, offset hashing or per-checkpoint printing in this arm.
 * The budget and syscall placement are still every 10,000 block entries. */
static void fast_checkpoint(void) {
    state_t *s = drmgr_get_tls_field(dr_get_current_drcontext(), tls);
    *s->raw_remaining = BUDGET;
    s->checkpoints++;
    if (progress)
        charge_progress();
}
static void thread_init(void *ctx) {
    state_t *s = dr_thread_alloc(ctx, sizeof(*s));
    memset(s, 0, sizeof(*s));
    s->remaining = BUDGET;
    s->digest = UINT64_C(14695981039346656037);
    if (fast) {
        s->raw_remaining = (reg_t *)((byte *)dr_get_dr_segment_base(raw_segment) + raw_offset);
        *s->raw_remaining = BUDGET;
    }
    drmgr_set_tls_field(ctx, tls, s);
}
static void thread_exit(void *ctx) {
    state_t *s = drmgr_get_tls_field(ctx, tls);
    if (fast) {
        dr_fprintf(STDERR, "budget: fast checkpoints=%llu\n",
                   (unsigned long long)s->checkpoints);
        dr_thread_free(ctx, s, sizeof(*s));
        return;
    }
    dr_fprintf(STDERR, "budget: checkpoints=%llu blocks=%llu digest=%llx anonymous=%llu\n",
               (unsigned long long)s->checkpoints,
               (unsigned long long)(s->checkpoints * BUDGET + BUDGET - s->remaining),
               (unsigned long long)s->digest,
               (unsigned long long)s->anonymous_checkpoints);
    dr_thread_free(ctx, s, sizeof(*s));
}
static dr_emit_flags_t instrument(void *ctx, void *tag, instrlist_t *bb,
                                 instr_t *inst, bool trace, bool translating,
                                 void *user) {
    if (!drmgr_is_first_instr(ctx, inst)) return DR_EMIT_DEFAULT;
    if (fast) {
        /* Direct segment-relative memory avoids loading the state pointer and
         * reserving an extra application GPR at every block. drreg still owns
         * arithmetic flags and clean-call application-state preservation. */
        DR_ASSERT(drreg_reserve_aflags(ctx, bb, inst) == DRREG_SUCCESS);
        instrlist_meta_preinsert(bb, inst, INSTR_CREATE_sub(ctx,
            dr_raw_tls_opnd(ctx, raw_segment, raw_offset), OPND_CREATE_INT8(1)));
        instr_t *skip = INSTR_CREATE_label(ctx);
        instrlist_meta_preinsert(bb, inst, INSTR_CREATE_jcc(ctx, OP_jnz, opnd_create_instr(skip)));
        dr_insert_clean_call(ctx, bb, inst, (void *)fast_checkpoint, false, 0);
        instrlist_meta_preinsert(bb, inst, skip);
        DR_ASSERT(drreg_unreserve_aflags(ctx, bb, inst) == DRREG_SUCCESS);
        return DR_EMIT_DEFAULT;
    }
    app_pc pc = instr_get_app_pc(inst);
    module_data_t *mod = dr_lookup_module(pc);
    ptr_uint_t offset = mod ? (ptr_uint_t)(pc - mod->start) : 0;
    int anonymous = mod == NULL;
    if (mod) dr_free_module_data(mod);
    reg_id_t reg;
    DR_ASSERT(drreg_reserve_aflags(ctx, bb, inst) == DRREG_SUCCESS);
    DR_ASSERT(drreg_reserve_register(ctx, bb, inst, NULL, &reg) == DRREG_SUCCESS);
    drmgr_insert_read_tls_field(ctx, tls, bb, inst, reg);
    instrlist_meta_preinsert(bb, inst, INSTR_CREATE_sub(ctx,
        opnd_create_base_disp(reg, DR_REG_NULL, 0, offsetof(state_t, remaining), OPSZ_4),
        OPND_CREATE_INT8(1)));
    instr_t *skip = INSTR_CREATE_label(ctx);
    instrlist_meta_preinsert(bb, inst, INSTR_CREATE_jcc(ctx, OP_jnz, opnd_create_instr(skip)));
    dr_insert_clean_call(ctx, bb, inst, (void *)checkpoint, false, 3,
                         OPND_CREATE_INTPTR(pc), OPND_CREATE_INTPTR(offset),
                         OPND_CREATE_INT32(anonymous));
    instrlist_meta_preinsert(bb, inst, skip);
    DR_ASSERT(drreg_unreserve_register(ctx, bb, inst, reg) == DRREG_SUCCESS);
    DR_ASSERT(drreg_unreserve_aflags(ctx, bb, inst) == DRREG_SUCCESS);
    return DR_EMIT_DEFAULT;
}
static void finished(void) {
    if (fast)
        DR_ASSERT(dr_raw_tls_cfree(raw_offset, 1));
    drmgr_unregister_tls_field(tls);
    drreg_exit();
    drmgr_exit();
}
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[]) {
    for (int i = 1; i < argc; ++i) {
        if (option_is(argv[i], "--progress"))
            progress = true;
        else if (option_is(argv[i], "--fast"))
            fast = true;
        else
            DR_ASSERT(false);
    }
    drreg_options_t options = {sizeof(options), 3, false};
    DR_ASSERT(drmgr_init());
    DR_ASSERT(drreg_init(&options) == DRREG_SUCCESS);
    if (fast)
        DR_ASSERT(dr_raw_tls_calloc(&raw_segment, &raw_offset, 1, 0));
    tls = drmgr_register_tls_field();
    DR_ASSERT(tls >= 0);
    drmgr_register_thread_init_event(thread_init);
    drmgr_register_thread_exit_event(thread_exit);
    drmgr_register_bb_instrumentation_event(NULL, instrument, NULL);
    dr_register_exit_event(finished);
}
