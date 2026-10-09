/* Automatic application progress, including shared libraries and JIT code.
 * Each thread charges one PV syscall before every 10,000th translated block.
 * The budget is part of execution semantics, not a performance setting.
 */
#include "dr_api.h"
#include "drmgr.h"
#include "drreg.h"

#define BLOCK_BUDGET 10000
static reg_id_t budget_segment;
static uint budget_offset;

static reg_t *budget(void)
{
    return (reg_t *)((byte *)dr_get_dr_segment_base(budget_segment) + budget_offset);
}

static void checkpoint(void)
{
    *budget() = BLOCK_BUDGET;
#ifdef X86_64
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(39L)
                     : "rcx", "r11", "memory", "cc");
#elif defined(AARCH64)
    register long number __asm__("x8") = 172;
    register long result __asm__("x0");
    __asm__ volatile("svc #0" : "=r"(result) : "r"(number) : "memory", "cc");
#else
#error Unsupported progress architecture
#endif
    DR_ASSERT(result > 0);
}

static void thread_init(void *context)
{
    (void)context;
    *budget() = BLOCK_BUDGET;
}

static dr_emit_flags_t instrument(void *context, void *tag, instrlist_t *block,
                                 instr_t *instruction, bool trace,
                                 bool translating, void *user)
{
    if (!drmgr_is_first_instr(context, instruction))
        return DR_EMIT_DEFAULT;
    instr_t *skip = INSTR_CREATE_label(context);
#ifdef X86_64
    DR_ASSERT(drreg_reserve_aflags(context, block, instruction) == DRREG_SUCCESS);
    instrlist_meta_preinsert(block, instruction, INSTR_CREATE_sub(context,
        dr_raw_tls_opnd(context, budget_segment, budget_offset), OPND_CREATE_INT8(1)));
    instrlist_meta_preinsert(block, instruction,
        INSTR_CREATE_jcc(context, OP_jnz, opnd_create_instr(skip)));
#elif defined(AARCH64)
    /* Non-flag-setting arithmetic and CBNZ preserve application NZCV. */
    reg_id_t scratch;
    DR_ASSERT(drreg_reserve_register(context, block, instruction, NULL, &scratch)
              == DRREG_SUCCESS);
    dr_insert_read_raw_tls(context, block, instruction, budget_segment,
                           budget_offset, scratch);
    instrlist_meta_preinsert(block, instruction, INSTR_CREATE_sub(context,
        opnd_create_reg(scratch), opnd_create_reg(scratch), OPND_CREATE_INT(1)));
    dr_insert_write_raw_tls(context, block, instruction, budget_segment,
                            budget_offset, scratch);
    instrlist_meta_preinsert(block, instruction, INSTR_CREATE_cbnz(context,
        opnd_create_instr(skip), opnd_create_reg(scratch)));
#endif
    dr_insert_clean_call(context, block, instruction, (void *)checkpoint, false, 0);
    instrlist_meta_preinsert(block, instruction, skip);
#ifdef X86_64
    DR_ASSERT(drreg_unreserve_aflags(context, block, instruction) == DRREG_SUCCESS);
#else
    DR_ASSERT(drreg_unreserve_register(context, block, instruction, scratch)
              == DRREG_SUCCESS);
#endif
    return DR_EMIT_DEFAULT;
}

static void finished(void)
{
    DR_ASSERT(dr_raw_tls_cfree(budget_offset, 1));
    drreg_exit();
    drmgr_exit();
}

DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[])
{
    drreg_options_t options = {sizeof(options), 3, false};
    DR_ASSERT(drmgr_init());
    DR_ASSERT(drreg_init(&options) == DRREG_SUCCESS);
    DR_ASSERT(dr_raw_tls_calloc(&budget_segment, &budget_offset, 1, 0));
    drmgr_register_thread_init_event(thread_init);
    drmgr_register_bb_instrumentation_event(NULL, instrument, NULL);
    dr_register_exit_event(finished);
}
