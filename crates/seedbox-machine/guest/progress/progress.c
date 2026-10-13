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
#include "drreg.h"

#include "budget.h"

static reg_t block_budget;
static reg_id_t budget_segment;
static uint budget_offset;
#ifdef X86_64
static ptr_int_t entropy_note;
#endif

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
        /* The clean call reads and writes application state: put every value
         * drreg still holds back first, so no later lazy restore overwrites
         * the replacement destination or flags. */
        DR_ASSERT(drreg_restore_all(context, block, instruction) == DRREG_SUCCESS);
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
    DR_ASSERT(dr_raw_tls_cfree(budget_offset, 1));
    drreg_exit();
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
    drreg_options_t options = {sizeof(options), 3, false};
    DR_ASSERT(drmgr_init());
    DR_ASSERT(drreg_init(&options) == DRREG_SUCCESS);
    DR_ASSERT(dr_raw_tls_calloc(&budget_segment, &budget_offset, 1, 0));
    drmgr_register_thread_init_event(thread_init);
#ifdef X86_64
    entropy_note = drmgr_reserve_note_range(1);
    DR_ASSERT(entropy_note != DRMGR_NOTE_NONE);
#endif
    drmgr_register_bb_app2app_event(rewrite, NULL);
    drmgr_register_bb_instrumentation_event(NULL, instrument, NULL);
    dr_register_exit_event(finished);
}
