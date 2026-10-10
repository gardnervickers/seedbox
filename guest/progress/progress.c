/* Automatic application progress, including shared libraries and JIT code.
 * Each thread charges one PV syscall per 10,000 counted block entries.
 * The budget is part of execution semantics, not a performance setting.
 *
 * On AArch64 the patched translator implements software exclusive reservations.
 * Host-only retries stay inside one mangled store instruction, below this
 * client's block accounting. Guest boundaries may invalidate a reservation,
 * including a progress syscall between the application's load and store.
 *
 * The exclusive monitor is not the only state a host interruption touches: a
 * host exception also wakes WFE/WFI, so a spin loop that waits would iterate,
 * and be charged, a host-dependent number of times. With one guest CPU and
 * interrupts delivered at deterministic rendezvous points, waiting gains the
 * application nothing, so this client rewrites those hints to NOP before the
 * translator sees them.
 */
#include "dr_api.h"
#include "drmgr.h"
#include "drreg.h"

#define BLOCK_BUDGET 10000
/* Experiment branch: the first client argument overrides the budget; 0 makes
 * the client empty (no instrumentation, no charges). */
static reg_t block_budget = BLOCK_BUDGET;
static reg_id_t budget_segment;
static uint budget_offset;

static reg_t *budget(void)
{
    return (reg_t *)((byte *)dr_get_dr_segment_base(budget_segment) + budget_offset);
}

static void checkpoint(void)
{
    *budget() = block_budget;
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
    *budget() = block_budget;
}

/* Insert the budget decrement and checkpoint call before `where`. */
static void charge_before(void *context, instrlist_t *block, instr_t *where)
{
    instr_t *skip = INSTR_CREATE_label(context);
#ifdef X86_64
    DR_ASSERT(drreg_reserve_aflags(context, block, where) == DRREG_SUCCESS);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_sub(context,
        dr_raw_tls_opnd(context, budget_segment, budget_offset), OPND_CREATE_INT8(1)));
    instrlist_meta_preinsert(block, where,
        INSTR_CREATE_jcc(context, OP_jnz, opnd_create_instr(skip)));
#elif defined(AARCH64)
    /* Non-flag-setting arithmetic and CBNZ preserve application NZCV. */
    reg_id_t scratch;
    DR_ASSERT(drreg_reserve_register(context, block, where, NULL, &scratch)
              == DRREG_SUCCESS);
    dr_insert_read_raw_tls(context, block, where, budget_segment, budget_offset, scratch);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_sub(context,
        opnd_create_reg(scratch), opnd_create_reg(scratch), OPND_CREATE_INT(1)));
    dr_insert_write_raw_tls(context, block, where, budget_segment, budget_offset, scratch);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_cbnz(context,
        opnd_create_instr(skip), opnd_create_reg(scratch)));
#endif
    dr_insert_clean_call(context, block, where, (void *)checkpoint, false, 0);
    instrlist_meta_preinsert(block, where, skip);
#ifdef X86_64
    DR_ASSERT(drreg_unreserve_aflags(context, block, where) == DRREG_SUCCESS);
#else
    DR_ASSERT(drreg_unreserve_register(context, block, where, scratch) == DRREG_SUCCESS);
#endif
}

/* Replace application wait hints with NOP, keeping the original translation so
 * faults and signals in the block still map to the application PC. */
static dr_emit_flags_t neutralize_waits(void *context, void *tag, instrlist_t *block,
                                        bool trace, bool translating)
{
#ifdef AARCH64
    for (instr_t *instruction = instrlist_first_app(block); instruction != NULL;
         instruction = instr_get_next_app(instruction)) {
        int opcode = instr_get_opcode(instruction);
        if (opcode != OP_wfe && opcode != OP_wfi && opcode != OP_wfet && opcode != OP_wfit)
            continue;
        instr_t *nop = INSTR_CREATE_nop(context);
        instr_set_translation(nop, instr_get_app_pc(instruction));
        instrlist_replace(block, instruction, nop);
        instr_destroy(context, instruction);
        instruction = nop;
    }
#endif
    return DR_EMIT_DEFAULT;
}

static dr_emit_flags_t instrument(void *context, void *tag, instrlist_t *block,
                                 instr_t *instruction, bool trace,
                                 bool translating, void *user)
{
    if (drmgr_is_first_instr(context, instruction))
        charge_before(context, block, instruction);
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
    if (argc > 1) {
        reg_t value = 0;
        for (const char *c = argv[1]; *c >= '0' && *c <= '9'; c++) value = value * 10 + (reg_t)(*c - '0');
        block_budget = value;
    }
    if (block_budget == 0) return; /* empty client: translation only */
    DR_ASSERT(drmgr_init());
    DR_ASSERT(drreg_init(&options) == DRREG_SUCCESS);
    DR_ASSERT(dr_raw_tls_calloc(&budget_segment, &budget_offset, 1, 0));
    drmgr_register_thread_init_event(thread_init);
    drmgr_register_bb_app2app_event(neutralize_waits, NULL);
    drmgr_register_bb_instrumentation_event(NULL, instrument, NULL);
    dr_register_exit_event(finished);
}
