/* Automatic application progress, including shared libraries and JIT code.
 * Each thread charges one PV syscall per configured number of executed
 * application instructions. Every translated block is charged its own
 * instruction count at one or more charge points; a charge that crosses the
 * budget makes a progress call, and the overshoot carries into the next
 * window. The budget and the charge points are part of execution semantics,
 * not performance settings.
 *
 * A progress call is a getpid whose first argument is the progress mark. The
 * kernel's PV syscall charge recognizes the mark and may add seeded progress
 * jitter there, drawn from a stream the VMM seeds per machine; the client
 * itself draws nothing.
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

#include "budget.h"

static ptr_int_t instruction_budget;
static uint points_per_block;
static reg_id_t budget_segment;
static uint budget_offset;

/* Instructions left in this thread's window. It goes negative when a charge
 * crosses the budget, and the checkpoint carries that overshoot. */
static ptr_int_t *budget(void)
{
    return (ptr_int_t *)((byte *)dr_get_dr_segment_base(budget_segment) + budget_offset);
}

#ifdef SEEDBOX_PARKED_PROGRESS_DIAGNOSTIC
#include "fixtures/parked-progress-hooks.h"
#endif

static ptr_int_t progress_call(void)
{
#ifdef X86_64
    long result;
    __asm__ volatile("syscall" : "=a"(result)
                     : "a"(39L), "D"((long)SEEDBOX_PROGRESS_MARK)
                     : "rcx", "r11", "memory", "cc");
#elif defined(AARCH64)
    register long number __asm__("x8") = 172;
    register long result __asm__("x0") = (long)SEEDBOX_PROGRESS_MARK;
    __asm__ volatile("svc #0" : "+r"(result) : "r"(number) : "memory", "cc");
#else
#error Unsupported progress architecture
#endif
#ifdef SEEDBOX_PARKED_PROGRESS_DIAGNOSTIC
    result = parked_progress_after_payment(result);
#endif
    return result;
}

/* One progress call per budget the charges have used, so a charge larger than
 * the budget still advances virtual time in proportion to its instructions. */
static void checkpoint(void)
{
    ptr_int_t *left = budget();
    do {
        *left += instruction_budget;
        ptr_int_t result = progress_call();
        DR_ASSERT(result > 0);
    } while (*left < 0);
}

#ifdef AARCH64
/* The core owns the finite payment on a fresh dispatcher stack. Its record
 * captures this old charge before publication can change the app block. */
static void checkpoint_and_dispatch(app_pc resume, ptr_int_t charged)
{
    dr_mcontext_t context = {sizeof(context), DR_MC_ALL};
    DR_ASSERT(dr_get_mcontext(dr_get_current_drcontext(), &context));
    context.pc = resume;
#ifdef SEEDBOX_PARKED_PROGRESS_DIAGNOSTIC
    if (parked_progress_diag != NULL && parked_progress_diag->armed) {
        if (parked_progress_failure == 5) instruction_budget = 0;
        if (parked_progress_failure == 6)
            dr_app_progress_exit(&context, (ptr_int_t *)&parked_progress_diag->initial_budget,
                                 instruction_budget, charged, progress_call);
        if (parked_progress_failure == 7) charged = 0;
        if (parked_progress_failure == 8) *budget() = 0;
    }
#endif
    dr_app_progress_exit(&context, budget(), instruction_budget, charged, progress_call);
    DR_ASSERT(false);
}
#endif

static void thread_init(void *context)
{
    (void)context;
    *budget() = instruction_budget;
}

/* Charge `instructions` before `where` and call the checkpoint when the charge
 * leaves the budget negative. Instrumentation preserves application flags. */
static void charge_before(void *context, instrlist_t *block, instr_t *where,
                          uint instructions)
{
#ifdef SEEDBOX_PARKED_PROGRESS_DIAGNOSTIC
    parked_progress_instrument(context, block, where, instructions);
#endif
    instr_t *skip = INSTR_CREATE_label(context);
#ifdef X86_64
    DR_ASSERT(drreg_reserve_aflags(context, block, where) == DRREG_SUCCESS);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_sub(context,
        dr_raw_tls_opnd(context, budget_segment, budget_offset),
        instructions <= 127 ? OPND_CREATE_INT8(instructions)
                            : OPND_CREATE_INT32(instructions)));
    instrlist_meta_preinsert(block, where,
        INSTR_CREATE_jcc(context, OP_jns, opnd_create_instr(skip)));
#elif defined(AARCH64)
    /* Non-flag-setting arithmetic and TBZ preserve application NZCV. */
    reg_id_t scratch;
    DR_ASSERT(drreg_reserve_register(context, block, where, NULL, &scratch)
              == DRREG_SUCCESS);
    dr_insert_read_raw_tls(context, block, where, budget_segment, budget_offset, scratch);
    /* SUB takes a 12-bit immediate. */
    for (uint left = instructions; left > 0;) {
        uint step = left < 4095 ? left : 4095;
        instrlist_meta_preinsert(block, where, INSTR_CREATE_sub(context,
            opnd_create_reg(scratch), opnd_create_reg(scratch), OPND_CREATE_INT(step)));
        left -= step;
    }
    dr_insert_write_raw_tls(context, block, where, budget_segment, budget_offset, scratch);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_tbz(context,
        opnd_create_instr(skip), opnd_create_reg(scratch), OPND_CREATE_INT(63)));
#endif
#ifdef X86_64
    dr_insert_clean_call(context, block, where, (void *)checkpoint, false, 0);
#else
    /* The redirect reads the application context, and TBZ skips the call. */
    dr_insert_clean_call_ex(context, block, where, (void *)checkpoint_and_dispatch,
                            DR_CLEANCALL_READS_APP_CONTEXT | DR_CLEANCALL_MULTIPATH, 2,
                            OPND_CREATE_INTPTR((ptr_int_t)instr_get_app_pc(where)),
                            OPND_CREATE_INTPTR((ptr_int_t)instructions));
#endif
    instrlist_meta_preinsert(block, where, skip);
#ifdef X86_64
    DR_ASSERT(drreg_unreserve_aflags(context, block, where) == DRREG_SUCCESS);
#else
    /* A fault can expose a register that is dead on the straight-line path.
     * Restore before the application instruction, not at its next normal read. */
    DR_ASSERT(drreg_get_app_value(context, block, where, scratch, scratch) == DRREG_SUCCESS);
    DR_ASSERT(drreg_unreserve_register(context, block, where, scratch) == DRREG_SUCCESS);
#endif
#ifdef SEEDBOX_PARKED_PROGRESS_DIAGNOSTIC
    parked_progress_instrument_after_charge(context, block, where);
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

/* One block's charge points, built when the block is analyzed and consumed
 * in order as its application instructions are instrumented. Point `p` starts
 * at instruction p * length / points and charges the instructions up to the
 * next point, so the points split the block as evenly as possible. */
typedef struct {
    uint length;
    uint points;
    uint index;
    uint next;
} charge_plan_t;

static uint point_start(const charge_plan_t *plan, uint point)
{
    return (uint)((uint64)point * plan->length / plan->points);
}

static dr_emit_flags_t analyze(void *context, void *tag, instrlist_t *block, bool trace,
                               bool translating, void **user)
{
    charge_plan_t *plan = dr_thread_alloc(context, sizeof(*plan));
    plan->length = 0;
    for (instr_t *instruction = instrlist_first_app(block); instruction != NULL;
         instruction = instr_get_next_app(instruction))
        plan->length++;
    plan->points = plan->length < points_per_block ? plan->length : points_per_block;
    plan->index = 0;
    plan->next = 0;
    *user = plan;
    return DR_EMIT_DEFAULT;
}

static dr_emit_flags_t instrument(void *context, void *tag, instrlist_t *block,
                                 instr_t *instruction, bool trace,
                                 bool translating, void *user)
{
    charge_plan_t *plan = user;
    if (instr_is_app(instruction)) {
        if (plan->next < plan->points && plan->index == point_start(plan, plan->next)) {
            plan->next++;
            charge_before(context, block, instruction,
                          point_start(plan, plan->next) - plan->index);
        }
        plan->index++;
    }
    if (drmgr_is_last_instr(context, instruction)) {
        DR_ASSERT(plan->index == plan->length && plan->next == plan->points);
        dr_thread_free(context, plan, sizeof(*plan));
    }
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
#ifdef SEEDBOX_PARKED_PROGRESS_DIAGNOSTIC
    if (argc != 6 && argc != 7) dr_abort();
    if (argc == 7) {
        if (argv[6][0] < '1' || argv[6][0] > '8' || argv[6][1] != 0) dr_abort();
        parked_progress_failure = argv[6][0] - '0';
    }
    parked_progress_target_path = argv[5];
    argc = 5;
#endif
    uint32_t parsed_budget, parsed_points;
    if (!seedbox_progress_arguments(argc, argv, &parsed_budget, &parsed_points)) {
        dr_fprintf(STDERR, "seedbox progress: expected " SEEDBOX_PROGRESS_INSTRUCTIONS
                   " and " SEEDBOX_PROGRESS_POINTS ", each with a positive u32\n");
        dr_abort();
        return;
    }
    /* Both supported ISAs use a 64-bit raw TLS word. Never encode the budget
     * as an instruction immediate: the entire u32 range fits in ptr_int_t. */
    instruction_budget = (ptr_int_t)parsed_budget;
    points_per_block = parsed_points;
    drreg_options_t options = {sizeof(options), 3, false};
#ifdef AARCH64
    /* Normal-path liveness does not describe exception-state liveness. */
    options.conservative = true;
#endif
    DR_ASSERT(drmgr_init());
    DR_ASSERT(drreg_init(&options) == DRREG_SUCCESS);
    DR_ASSERT(dr_raw_tls_calloc(&budget_segment, &budget_offset, 1, 0));
#ifdef SEEDBOX_PARKED_PROGRESS_DIAGNOSTIC
    drmgr_register_signal_event(parked_progress_signal);
#endif
    drmgr_register_thread_init_event(thread_init);
    drmgr_register_bb_app2app_event(neutralize_waits, NULL);
    drmgr_register_bb_instrumentation_event(analyze, instrument, NULL);
    /* The newer ARM SDK routes exit callbacks through drmgr. */
#ifdef AARCH64
    drmgr_register_exit_event(finished);
#else
    dr_register_exit_event(finished);
#endif
}
