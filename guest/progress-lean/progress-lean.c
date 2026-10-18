/* The lean AArch64 progress client, selected per simulation with
 * `engine: {progressClient: lean}`. It charges exactly what progress.c
 * charges: the same blocks, points and amounts, the same budget carry and the
 * same exit through the dispatcher. Only the instrumentation differs, which
 * makes short-lived processes start about 2.5 times faster: progress.c's
 * drreg liveness analysis and the clean call it emits into every block
 * dominated their start-up. The translator's own memory commits are marked
 * syscalls, so the smaller code cache changes simulated time slightly; that is
 * why this is a separate client and not a replacement.
 *
 * Each charge saves one fixed scratch register to its own raw TLS slot, counts
 * the budget down with non-flag-setting arithmetic and TBZ, and reloads the
 * register before the charged application instruction, so no application
 * instruction ever runs while the register holds the budget. A charge that
 * crosses the budget branches to one slow path emitted at start-up.
 *
 * See progress.c for the progress call, wait hints and exclusive reservations.
 */
#include "dr_api.h"
#include "drmgr.h"

#include "budget.h"

#ifndef AARCH64
#error The lean progress client is AArch64-only
#endif

static ptr_int_t instruction_budget;
static uint points_per_block;
static reg_id_t budget_segment;
/* Raw TLS: the budget, the saved scratch register, then the slow path's
 * resume address and charge. */
static uint budget_offset, spill_offset, resume_offset, charged_offset;
/* Any register but DR's stolen x28 works; the choice changes no application
 * value. */
#define SCRATCH DR_REG_X1
/* Shared slow path: reload the scratch register, then a clean call. */
static byte *dispatch_routine;
static size_t dispatch_routine_size;
/* Each thread's raw TLS base, so a translation on another thread can read the
 * saved register. */
static int segment_field;
/* Marks the charge instructions during which SCRATCH holds the budget. */
static ptr_uint_t holds_budget;

/* Instructions left in this thread's window. It goes negative when a charge
 * crosses the budget, and the checkpoint carries that overshoot. */
static ptr_int_t *budget(void)
{
    return (ptr_int_t *)((byte *)dr_get_dr_segment_base(budget_segment) + budget_offset);
}

static void progress_call(void)
{
    register long number __asm__("x8") = 172;
    register long result __asm__("x0") = (long)SEEDBOX_PROGRESS_MARK;
    __asm__ volatile("svc #0" : "+r"(result) : "r"(number) : "memory", "cc");
    DR_ASSERT(result > 0);
}

/* One progress call per budget the charges have used, so a charge larger than
 * the budget still advances virtual time in proportion to its instructions. */
static void checkpoint(void)
{
    ptr_int_t *left = budget();
    do {
        *left += instruction_budget;
        progress_call();
    } while (*left < 0);
}

/* As in progress.c, leave through the dispatcher, which waits for a pending
 * synchronization at a safe spot. The charge stored the application address
 * of the charged instruction and its charge; the charge there runs again, so
 * give it back. */
static void checkpoint_and_dispatch(void)
{
    byte *base = dr_get_dr_segment_base(budget_segment);
    app_pc resume = *(app_pc *)(base + resume_offset);
    ptr_int_t charged = *(ptr_int_t *)(base + charged_offset);
    checkpoint();
    *budget() += charged;
    dr_mcontext_t context = {sizeof(context), DR_MC_ALL};
    DR_ASSERT(dr_get_mcontext(dr_get_current_drcontext(), &context));
    context.pc = resume;
    dr_redirect_execution(&context);
    DR_ASSERT(false);
}

/* Emit the slow path once. A block enters it with the scratch register saved
 * and every other application register intact. It sits in DR's own memory,
 * where DR never relocates a thread, just as it never relocates one inside the
 * clean call's callee. */
static void emit_dispatch_routine(void *context)
{
    instrlist_t *routine = instrlist_create(context);
    instr_t *end = INSTR_CREATE_label(context);
    instrlist_meta_append(routine, end);
    dr_insert_read_raw_tls(context, routine, end, budget_segment, spill_offset, SCRATCH);
    dr_insert_clean_call_ex(context, routine, end, (void *)checkpoint_and_dispatch,
                            DR_CLEANCALL_READS_APP_CONTEXT | DR_CLEANCALL_MULTIPATH, 0);
    dispatch_routine_size = dr_page_size();
    dispatch_routine = dr_nonheap_alloc(dispatch_routine_size, DR_MEMPROT_READ |
                                        DR_MEMPROT_WRITE | DR_MEMPROT_EXEC);
    byte *last = instrlist_encode(context, routine, dispatch_routine, true);
    DR_ASSERT(last != NULL && last <= dispatch_routine + dispatch_routine_size);
    instrlist_clear_and_destroy(context, routine);
    /* Make the new code visible to instruction fetch. */
    for (byte *line = dispatch_routine; line < last; line += 4)
        __asm__ volatile("dc cvau, %0" : : "r"(line) : "memory");
    __asm__ volatile("dsb ish" : : : "memory");
    for (byte *line = dispatch_routine; line < last; line += 4)
        __asm__ volatile("ic ivau, %0" : : "r"(line) : "memory");
    __asm__ volatile("dsb ish\n\tisb" : : : "memory");
}

static void thread_init(void *context)
{
    /* The first thread runs alone, before any block is built. */
    if (dispatch_routine == NULL)
        emit_dispatch_routine(context);
    drmgr_set_tls_field(context, segment_field, dr_get_dr_segment_base(budget_segment));
    *budget() = instruction_budget;
}

/* The charge's instructions have no translation, so DR translates a thread
 * there only to report its PC and never relocates it. Still put the
 * application's register back when DR recreated the block's instructions; a
 * translation from stored tables (no ilist) cannot see the charge and leaves
 * the register as it found it. */
static bool restore_scratch(void *context, bool restore_memory, dr_restore_state_info_t *info)
{
    (void)restore_memory;
    byte *pc = info->fragment_info.cache_start_pc;
    if (info->fragment_info.ilist == NULL || pc == NULL || !info->raw_mcontext_valid)
        return true;
    for (instr_t *instruction = instrlist_first(info->fragment_info.ilist);
         instruction != NULL && pc <= info->raw_mcontext->pc;
         instruction = instr_get_next(instruction)) {
        int length = instr_length(context, instruction);
        if (length == 0)
            continue;
        if (pc == info->raw_mcontext->pc) {
            if (instr_get_note(instruction) == (void *)holds_budget) {
                byte *base = drmgr_get_tls_field(context, segment_field);
                reg_set_value(SCRATCH, info->mcontext, *(reg_t *)(base + spill_offset));
            }
            break;
        }
        pc += length;
    }
    return true;
}

/* Charge `instructions` before `where` and branch to the slow path when the
 * charge leaves the budget negative. The sequence depends only on the block,
 * so DR's recreation of it for translation matches. */
static void charge_before(void *context, instrlist_t *block, instr_t *where,
                          uint instructions)
{
    instr_t *skip = INSTR_CREATE_label(context);
    dr_insert_write_raw_tls(context, block, where, budget_segment, spill_offset, SCRATCH);
    instr_t *saved = instr_get_prev(where);
    dr_insert_read_raw_tls(context, block, where, budget_segment, budget_offset, SCRATCH);
    /* SUB takes a 12-bit immediate. */
    for (uint left = instructions; left > 0;) {
        uint step = left < 4095 ? left : 4095;
        instrlist_meta_preinsert(block, where, INSTR_CREATE_sub(context,
            opnd_create_reg(SCRATCH), opnd_create_reg(SCRATCH), OPND_CREATE_INT(step)));
        left -= step;
    }
    dr_insert_write_raw_tls(context, block, where, budget_segment, budget_offset, SCRATCH);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_tbz(context,
        opnd_create_instr(skip), opnd_create_reg(SCRATCH), OPND_CREATE_INT(63)));
    /* Hand the slow path the charged instruction's application address and
     * the charge, and branch there; it never returns. */
    instrlist_insert_mov_immed_ptrsz(context, (ptr_int_t)instructions,
                                     opnd_create_reg(SCRATCH), block, where, NULL, NULL);
    dr_insert_write_raw_tls(context, block, where, budget_segment, charged_offset, SCRATCH);
    instrlist_insert_mov_immed_ptrsz(context, (ptr_int_t)instr_get_app_pc(where),
                                     opnd_create_reg(SCRATCH), block, where, NULL, NULL);
    dr_insert_write_raw_tls(context, block, where, budget_segment, resume_offset, SCRATCH);
    instrlist_insert_mov_immed_ptrsz(context, (ptr_int_t)dispatch_routine,
                                     opnd_create_reg(SCRATCH), block, where, NULL, NULL);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_br(context, opnd_create_reg(SCRATCH)));
    instrlist_meta_preinsert(block, where, skip);
    /* Give the scratch register its application value back before `where`:
     * see progress.c for why a lazy restore exposed the budget to HotSpot. */
    dr_insert_read_raw_tls(context, block, where, budget_segment, spill_offset, SCRATCH);
    for (instr_t *held = instr_get_next(saved); held != where; held = instr_get_next(held))
        instr_set_note(held, (void *)holds_budget);
}

/* As progress.c: replace application wait hints with NOP, keeping the original
 * translation so faults and signals in the block still map to the
 * application PC. */
static dr_emit_flags_t neutralize_waits(void *context, void *tag, instrlist_t *block,
                                        bool trace, bool translating)
{
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
    return DR_EMIT_DEFAULT;
}

/* The same charge plan as progress.c: point `p` starts at instruction
 * p * length / points and charges the instructions up to the next point. */
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
    DR_ASSERT(dr_raw_tls_cfree(budget_offset, 4));
    if (dispatch_routine != NULL)
        dr_nonheap_free(dispatch_routine, dispatch_routine_size);
    dispatch_routine = NULL;
    drmgr_unregister_tls_field(segment_field);
    drmgr_exit();
}

DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[])
{
    uint32_t parsed_budget, parsed_points;
    const char *coverage;
    if (!seedbox_progress_arguments(argc, argv, &parsed_budget, &parsed_points, &coverage)) {
        dr_fprintf(STDERR, "seedbox progress: expected " SEEDBOX_PROGRESS_INSTRUCTIONS
                   " and " SEEDBOX_PROGRESS_POINTS ", each with a positive u32\n");
        dr_abort();
        return;
    }
    if (coverage) {
        dr_fprintf(STDERR, "seedbox progress: the lean client records no block coverage\n");
        dr_abort();
        return;
    }
    instruction_budget = (ptr_int_t)parsed_budget;
    points_per_block = parsed_points;
    /* drmgr registers DR's events, such as for signals and thread exits,
     * whose presence alters DR's own behavior, as for progress.c. */
    DR_ASSERT(drmgr_init());
    DR_ASSERT(dr_raw_tls_calloc(&budget_segment, &budget_offset, 4, 0));
    spill_offset = budget_offset + sizeof(reg_t);
    resume_offset = spill_offset + sizeof(reg_t);
    charged_offset = resume_offset + sizeof(reg_t);
    segment_field = drmgr_register_tls_field();
    DR_ASSERT(segment_field != -1);
    holds_budget = drmgr_reserve_note_range(1);
    DR_ASSERT(holds_budget != DRMGR_NOTE_NONE);
    /* progress.c's drreg registers one too, so DR keeps calling the client
     * when it translates. */
    DR_ASSERT(drmgr_register_restore_state_ex_event(restore_scratch));
    drmgr_register_thread_init_event(thread_init);
    drmgr_register_bb_app2app_event(neutralize_waits, NULL);
    drmgr_register_bb_instrumentation_event(analyze, instrument, NULL);
    drmgr_register_exit_event(finished);
}
