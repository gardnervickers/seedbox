/* Automatic application progress, including shared libraries and JIT code.
 * Each thread charges one PV syscall per configured number of counted block entries.
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
 *
 * On x86-64 the machine hides RDRAND and RDSEED from CPUID, but an AMD host
 * cannot trap them: code that executes them regardless would read host
 * entropy. This client replaces every RDRAND and RDSEED, of each operand size,
 * with a draw from the guest kernel's getrandom, which is seeded from the
 * machine's RNG stream. The result always succeeds: CF is set, OF, SF, ZF, AF
 * and PF are cleared, and the destination is written with the architectural
 * width (a 32-bit write zero-extends, a 16-bit write keeps the upper bits).
 * Each draw is one guest syscall and is charged like any other.
 */
#include "dr_api.h"
#include "drmgr.h"

#include "budget.h"

static reg_t block_budget;
static reg_id_t budget_segment;
/* One fixed scratch avoids drreg's per-block register-liveness analysis.
 * No application instruction executes until its original value is restored. */
#ifdef X86_64
#define SCRATCH DR_REG_RCX
#else
#define SCRATCH DR_REG_X1
#endif
enum { BUDGET, SPILL, RESUME, DISPATCH, TLS_WORDS };
static uint budget_offset;
static int segment_field;
static ptr_uint_t holds_scratch;
static byte *dispatch_routine;
static size_t dispatch_size;
#ifdef X86_64
static ptr_int_t entropy_note;
#endif

static reg_t *budget(void)
{
    return (reg_t *)((byte *)dr_get_dr_segment_base(budget_segment) + budget_offset);
}

static uint slot(uint word)
{
    return budget_offset + word * sizeof(reg_t);
}

static void checkpoint_and_dispatch(void)
{
    byte *base = dr_get_dr_segment_base(budget_segment);
    app_pc resume = *(app_pc *)(base + slot(RESUME));
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
    /* Leave through the dispatcher so a pending signal or JIT synchronization
     * can complete at a safe point. The resumed block charges again: refund
     * that entry, including when the configured budget is one or UINT32_MAX. */
    ++*budget();
    dr_mcontext_t machine = { sizeof(machine), DR_MC_ALL };
    DR_ASSERT(dr_get_mcontext(dr_get_current_drcontext(), &machine));
    machine.pc = resume;
    dr_redirect_execution(&machine);
    DR_ASSERT(false);
}

/* Emit the expensive context-save/call sequence once, outside the code cache.
 * It uses the executing thread's TLS and DR stack, not the emitter's context.
 * It never returns into a fragment that a synchronization may have deleted. */
static void emit_dispatch(void *context)
{
    instrlist_t *routine = instrlist_create(context);
    instr_t *end = INSTR_CREATE_label(context);
    instrlist_meta_append(routine, end);
    dr_insert_read_raw_tls(context, routine, end, budget_segment, slot(SPILL), SCRATCH);
    dr_insert_clean_call_ex(context, routine, end, (void *)checkpoint_and_dispatch,
        DR_CLEANCALL_READS_APP_CONTEXT | DR_CLEANCALL_ALWAYS_OUT_OF_LINE, 0);
    dispatch_size = dr_page_size();
    dispatch_routine = dr_nonheap_alloc(dispatch_size,
        DR_MEMPROT_READ | DR_MEMPROT_WRITE | DR_MEMPROT_EXEC);
    DR_ASSERT(dispatch_routine != NULL);
    size_t length = 0;
    for (instr_t *instruction = instrlist_first(routine); instruction != NULL;
         instruction = instr_get_next(instruction))
        length += instr_length(context, instruction);
    DR_ASSERT(length <= dispatch_size);
    byte *last = instrlist_encode(context, routine, dispatch_routine, true);
    DR_ASSERT(last != NULL && last <= dispatch_routine + dispatch_size);
    instrlist_clear_and_destroy(context, routine);
#ifdef AARCH64
    /* Publish generated instructions to AArch64's instruction cache. */
    for (byte *line = dispatch_routine; line < last; line += 4)
        __asm__ volatile("dc cvau, %0" : : "r"(line) : "memory");
    __asm__ volatile("dsb ish" : : : "memory");
    for (byte *line = dispatch_routine; line < last; line += 4)
        __asm__ volatile("ic ivau, %0" : : "r"(line) : "memory");
    __asm__ volatile("dsb ish\n\tisb" : : : "memory");
#endif
    DR_ASSERT(dr_memory_protect(dispatch_routine, dispatch_size,
                               DR_MEMPROT_READ | DR_MEMPROT_EXEC));
}

static void thread_init(void *context)
{
    /* The initial thread initializes the routine before application execution. */
    if (dispatch_routine == NULL)
        emit_dispatch(context);
    byte *base = dr_get_dr_segment_base(budget_segment);
    drmgr_set_tls_field(context, segment_field, base);
    *budget() = block_budget;
    *(byte **)(base + slot(DISPATCH)) = dispatch_routine;
}

/* NULL translations keep the scratch window from being a relocation point.
 * Repair its reported register value when DR reconstructs an instruction list;
 * stored translation tables have no list and remain PC-only inside the window. */
static bool restore_scratch(void *context, bool restore_memory,
                            dr_restore_state_info_t *info)
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
            if (instr_get_note(instruction) == (void *)holds_scratch) {
                byte *base = drmgr_get_tls_field(context, segment_field);
                reg_set_value(SCRATCH, info->mcontext, *(reg_t *)(base + slot(SPILL)));
            }
            break;
        }
        pc += length;
    }
    return true;
}

/* Insert the budget decrement and checkpoint call before `where`. */
static void charge_before(void *context, instrlist_t *block, instr_t *where)
{
    instr_t *skip = INSTR_CREATE_label(context);
    dr_insert_write_raw_tls(context, block, where, budget_segment, slot(SPILL), SCRATCH);
    instr_t *saved = instr_get_prev(where);
    dr_insert_read_raw_tls(context, block, where, budget_segment, slot(BUDGET), SCRATCH);
#ifdef X86_64
    /* LEA and JRCXZ leave every application flag untouched, including OF/DF. */
    instrlist_meta_preinsert(block, where, INSTR_CREATE_lea(context,
        opnd_create_reg(SCRATCH),
        opnd_create_base_disp(SCRATCH, DR_REG_NULL, 0, -1, OPSZ_lea)));
    dr_insert_write_raw_tls(context, block, where, budget_segment, slot(BUDGET), SCRATCH);
    instr_t *slow = INSTR_CREATE_label(context);
    /* JRCXZ has only a short displacement: its target skips one jump. */
    instrlist_meta_preinsert(block, where,
        INSTR_CREATE_jecxz(context, opnd_create_instr(slow)));
    instrlist_meta_preinsert(block, where,
        INSTR_CREATE_jmp(context, opnd_create_instr(skip)));
    instrlist_meta_preinsert(block, where, slow);
#elif defined(AARCH64)
    /* Non-flag-setting arithmetic and CBNZ preserve application NZCV. */
    instrlist_meta_preinsert(block, where, INSTR_CREATE_sub(context,
        opnd_create_reg(SCRATCH), opnd_create_reg(SCRATCH), OPND_CREATE_INT(1)));
    dr_insert_write_raw_tls(context, block, where, budget_segment, slot(BUDGET), SCRATCH);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_cbnz(context,
        opnd_create_instr(skip), opnd_create_reg(SCRATCH)));
#endif
    /* The first instruction may be an entropy label without an application PC. */
    app_pc resume = instr_get_app_pc(instrlist_first_app(block));
    DR_ASSERT(resume != NULL);
    instrlist_insert_mov_immed_ptrsz(context, (ptr_int_t)resume, opnd_create_reg(SCRATCH),
                                     block, where, NULL, NULL);
    dr_insert_write_raw_tls(context, block, where, budget_segment, slot(RESUME), SCRATCH);
#ifdef X86_64
    instrlist_meta_preinsert(block, where, INSTR_CREATE_jmp_ind(context,
        dr_raw_tls_opnd(context, budget_segment, slot(DISPATCH))));
#else
    dr_insert_read_raw_tls(context, block, where, budget_segment, slot(DISPATCH), SCRATCH);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_br(context, opnd_create_reg(SCRATCH)));
#endif
    instrlist_meta_preinsert(block, where, skip);
    dr_insert_read_raw_tls(context, block, where, budget_segment, slot(SPILL), SCRATCH);
    for (instr_t *held = instr_get_next(saved); held != where; held = instr_get_next(held))
        instr_set_note(held, (void *)holds_scratch);
}

#ifdef X86_64
/* Deterministic replacement for RDRAND/RDSEED: guest getrandom, never host
 * entropy. Runs as a clean call, so every application register and flag is
 * saved; only the destination and the arithmetic flags change. */
static void entropy(reg_id_t destination, uint bytes)
{
    uint64 value = 0;
    for (size_t done = 0; done < sizeof(value);) {
        long result;
        __asm__ volatile("syscall" : "=a"(result)
                         : "a"(318L), "D"((byte *)&value + done),
                           "S"(sizeof(value) - done), "d"(0L)
                         : "rcx", "r11", "memory", "cc");
        DR_ASSERT(result > 0 || result == -4 /* EINTR */);
        if (result > 0)
            done += (size_t)result;
    }
    void *context = dr_get_current_drcontext();
    dr_mcontext_t machine = { sizeof(machine), DR_MC_INTEGER | DR_MC_CONTROL };
    DR_ASSERT(dr_get_mcontext(context, &machine));
    reg_id_t full = reg_to_pointer_sized(destination);
    reg_t written = (reg_t)value;
    if (bytes == 4)
        written = (uint)value;
    else if (bytes == 2)
        written = (reg_get_value(full, &machine) & ~(reg_t)0xffff) | (ushort)value;
    else
        DR_ASSERT(bytes == 8);
    reg_set_value(full, &machine, written);
    machine.xflags &= ~(reg_t)(EFLAGS_OF | EFLAGS_SF | EFLAGS_ZF | EFLAGS_AF | EFLAGS_PF);
    machine.xflags |= EFLAGS_CF;
    DR_ASSERT(dr_set_mcontext(context, &machine));
}
#endif

/* Rewrite nondeterministic application instructions before instrumentation.
 * Each replacement NOP keeps the original translation, so faults and signals
 * in the block still map to the application PC. */
static dr_emit_flags_t rewrite(void *context, void *tag, instrlist_t *block,
                               bool trace, bool translating)
{
    for (instr_t *instruction = instrlist_first_app(block); instruction != NULL;
         instruction = instr_get_next_app(instruction)) {
        int opcode = instr_get_opcode(instruction);
#ifdef AARCH64
        /* Wait hints become NOP. */
        if (opcode != OP_wfe && opcode != OP_wfi && opcode != OP_wfet && opcode != OP_wfit)
            continue;
#else
        /* Hardware entropy becomes a labelled NOP; the insertion pass puts the
         * guest-entropy clean call at the label. */
        if (opcode != OP_rdrand && opcode != OP_rdseed)
            continue;
        opnd_t destination = instr_get_dst(instruction, 0);
        DR_ASSERT(opnd_is_reg(destination));
        instr_t *label = INSTR_CREATE_label(context);
        instr_set_note(label, (void *)entropy_note);
        dr_instr_label_data_t *data = instr_get_label_data_area(label);
        data->data[0] = (ptr_uint_t)opnd_get_reg(destination);
        data->data[1] = (ptr_uint_t)opnd_size_in_bytes(opnd_get_size(destination));
        instrlist_preinsert(block, instruction, label);
#endif
        instr_t *nop = INSTR_CREATE_nop(context);
        instr_set_translation(nop, instr_get_app_pc(instruction));
        instrlist_replace(block, instruction, nop);
        instr_destroy(context, instruction);
        instruction = nop;
    }
    return DR_EMIT_DEFAULT;
}

static dr_emit_flags_t instrument(void *context, void *tag, instrlist_t *block,
                                 instr_t *instruction, bool trace,
                                 bool translating, void *user)
{
    if (drmgr_is_first_instr(context, instruction))
        charge_before(context, block, instruction);
#ifdef X86_64
    if (instr_is_label(instruction) && instr_get_note(instruction) == (void *)entropy_note) {
        /* The budget scratch is already restored before this call. */
        dr_instr_label_data_t *data = instr_get_label_data_area(instruction);
        dr_insert_clean_call(context, block, instruction, (void *)entropy, false, 2,
                             OPND_CREATE_INT32((int)data->data[0]),
                             OPND_CREATE_INT32((int)data->data[1]));
    }
#endif
    return DR_EMIT_DEFAULT;
}

static void finished(void)
{
    DR_ASSERT(dr_raw_tls_cfree(budget_offset, TLS_WORDS));
    if (dispatch_routine != NULL)
        dr_nonheap_free(dispatch_routine, dispatch_size);
    dispatch_routine = NULL;
    DR_ASSERT(drmgr_unregister_tls_field(segment_field));
    drmgr_exit();
}

DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[])
{
    uint32_t parsed_budget;
    if (argc != 3 || !seedbox_progress_budget(argv[1], argv[2], &parsed_budget)) {
        dr_fprintf(STDERR, "seedbox progress: expected --blocks-per-progress-call with a positive u32\n");
        dr_abort();
        return;
    }
    /* Both supported ISAs use a 64-bit raw TLS word. Never encode the budget
     * as an instruction immediate: the entire u32 range fits in reg_t. */
    block_budget = (reg_t)parsed_budget;
    DR_ASSERT(drmgr_init());
    DR_ASSERT(dr_raw_tls_calloc(&budget_segment, &budget_offset, TLS_WORDS, 0));
    segment_field = drmgr_register_tls_field();
    DR_ASSERT(segment_field != -1);
    holds_scratch = drmgr_reserve_note_range(1);
    DR_ASSERT(holds_scratch != DRMGR_NOTE_NONE);
    DR_ASSERT(drmgr_register_restore_state_ex_event(restore_scratch));
    drmgr_register_thread_init_event(thread_init);
#ifdef X86_64
    entropy_note = drmgr_reserve_note_range(1);
    DR_ASSERT(entropy_note != DRMGR_NOTE_NONE);
#endif
    drmgr_register_bb_app2app_event(rewrite, NULL);
    drmgr_register_bb_instrumentation_event(NULL, instrument, NULL);
    dr_register_exit_event(finished);
}
