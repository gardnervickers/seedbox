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
/* SCRATCH holds the budget while a block charges; its application value waits
 * in the SPILL slot. x86-64 always uses RCX (JRCXZ tests it without flags) and
 * restores it before the block's first application instruction. AArch64 picks
 * a register per block (choose_scratch): one the block overwrites before
 * reading needs no restore, so its first application instructions do not wait
 * on a reload.
 *
 * Every block's instrumentation keeps the same length whichever register it
 * uses: fragment sizes decide how the code cache grows, and DynamoRIO's own
 * syscalls for that growth are charged to the guest. */
enum {
    BUDGET,
    SPILL,
    RESUME,
#ifdef AARCH64
    /* The code cache entry at which the exhaustion routine resumes a block. */
    ENTRY,
#endif
    /* The exhaustion routine's entry. AArch64 has one word per X register:
     * DISPATCH + n holds the entry for a block whose scratch register is Xn. */
    DISPATCH,
#ifdef AARCH64
    TLS_WORDS = DISPATCH + 31
#else
    TLS_WORDS
#endif
};
static uint budget_offset;
static int segment_field;
/* Notes mark the instructions during which SCRATCH's application value is only
 * in SPILL, without allocating labels: DynamoRIO's own heap growth is charged
 * to the guest, so instrumentation must not change it. Each kind has one note
 * per register index (scratch_index). Charge instructions carry HELD, or SPANS
 * when the window continues through application instructions to the one that
 * overwrites a dead scratch register, which carries LAST. */
enum { HELD, SPANS, LAST, NOTE_KINDS, NOTE_REGISTERS = 32 };
static ptr_uint_t scratch_notes;
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

/* The first argument of a progress getpid. The guest kernel counts timer
 * ticks that interrupt such a call's charge as user time. */
#define SEEDBOX_PROGRESS_MARK 0x5eedb0c5L

#ifdef AARCH64
/* Preference order: intra-procedure scratch, temporaries, arguments, then
 * callee-saved registers. DR's stolen register is never a candidate. */
static const reg_id_t candidates[] = {
    DR_REG_X16, DR_REG_X17, DR_REG_X9,  DR_REG_X10, DR_REG_X11, DR_REG_X12, DR_REG_X13,
    DR_REG_X14, DR_REG_X15, DR_REG_X0,  DR_REG_X1,  DR_REG_X2,  DR_REG_X3,  DR_REG_X4,
    DR_REG_X5,  DR_REG_X6,  DR_REG_X7,  DR_REG_X8,  DR_REG_X18, DR_REG_X19, DR_REG_X20,
    DR_REG_X21, DR_REG_X22, DR_REG_X23, DR_REG_X24, DR_REG_X25, DR_REG_X26, DR_REG_X27,
    DR_REG_X29, DR_REG_X30,
};
static const size_t candidate_count = sizeof(candidates) / sizeof(candidates[0]);
#endif

static void checkpoint_and_dispatch(void)
{
    byte *base = dr_get_dr_segment_base(budget_segment);
    app_pc resume = *(app_pc *)(base + slot(RESUME));
    *budget() = block_budget;
#ifdef X86_64
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(39L), "D"(SEEDBOX_PROGRESS_MARK)
                     : "rcx", "r11", "memory", "cc");
#elif defined(AARCH64)
    register long number __asm__("x8") = 172;
    register long result __asm__("x0") = SEEDBOX_PROGRESS_MARK;
    __asm__ volatile("svc #0" : "+r"(result) : "r"(number) : "memory", "cc");
#else
#error Unsupported progress architecture
#endif
    DR_ASSERT(result > 0);
    /* The block resumes from its start and charges again: refund that entry,
     * including when the configured budget is one or UINT32_MAX. */
    ++*budget();
    void *context = dr_get_current_drcontext();
#ifdef AARCH64
    /* A pending signal or JIT synchronization must complete at this safe point,
     * which the dispatcher provides. When it has nothing to do, entering the
     * block's fragment directly has the same effect, for a fraction of the cost:
     * the routine restores the saved context and jumps to the entry. Only
     * AArch64 takes this path: DynamoRIO implements it there alone. */
    byte *entry = dr_redirect_entry(context, resume);
    if (entry != NULL) {
        *(byte **)(base + slot(ENTRY)) = entry;
        return;
    }
#endif
    dr_mcontext_t machine = { sizeof(machine), DR_MC_ALL };
    DR_ASSERT(dr_get_mcontext(context, &machine));
    machine.pc = resume;
    dr_redirect_execution(&machine);
    DR_ASSERT(false);
}

#ifdef AARCH64
/* Each X register's entry is two instructions at a fixed place in the page. */
enum { ENTRY_BYTES = 8, ENTRIES_BYTES = 31 * ENTRY_BYTES };

static byte *dispatch_entry(reg_id_t scratch)
{
    return dispatch_routine + (scratch - DR_REG_X0) * ENTRY_BYTES;
}
#endif

/* Emit the expensive context-save/call sequence once, outside the code cache.
 * It uses the executing thread's TLS and DR stack, not the emitter's context.
 * It goes back into the code cache only at an entry from dr_redirect_entry(),
 * which DR refuses while a synchronization, flush or signal is pending, and
 * through DR's own jump, which leaves for the dispatcher if a signal has
 * arrived since.
 *
 * An exhausted block left before restoring its scratch register, so the
 * routine restores it before the clean call saves the application's state.
 * x86-64 always restores RCX. An AArch64 block enters through its register's
 * own entry, which restores that register and branches to the shared call:
 * which register a block used is known from where it entered, never from
 * register values, which an application controls. */
static void emit_dispatch(void *context)
{
    instrlist_t *routine = instrlist_create(context);
    instr_t *end = INSTR_CREATE_label(context);
    instrlist_meta_append(routine, end);
#ifdef X86_64
    dr_insert_read_raw_tls(context, routine, end, budget_segment, slot(SPILL), DR_REG_RCX);
#endif
    dr_insert_clean_call_ex(context, routine, end, (void *)checkpoint_and_dispatch,
        DR_CLEANCALL_READS_APP_CONTEXT | DR_CLEANCALL_ALWAYS_OUT_OF_LINE, 0);
    dispatch_size = dr_page_size();
    dispatch_routine = dr_nonheap_alloc(dispatch_size,
        DR_MEMPROT_READ | DR_MEMPROT_WRITE | DR_MEMPROT_EXEC);
    DR_ASSERT(dispatch_routine != NULL);
    byte *shared = dispatch_routine;
#ifdef AARCH64
    shared += ENTRIES_BYTES;
#endif
    size_t length = 0;
    for (instr_t *instruction = instrlist_first(routine); instruction != NULL;
         instruction = instr_get_next(instruction))
        length += instr_length(context, instruction);
    DR_ASSERT(shared + length <= dispatch_routine + dispatch_size);
    byte *last = instrlist_encode(context, routine, shared, true);
    DR_ASSERT(last != NULL && last <= dispatch_routine + dispatch_size);
    instrlist_clear_and_destroy(context, routine);
#ifdef AARCH64
    /* Build each entry only after the shared call's list is destroyed, one at
     * a time, so its instructions reuse that list's freed memory: DynamoRIO's
     * heap growth is charged to the guest and must not depend on them. */
    for (size_t i = 0; i < candidate_count; i++) {
        instrlist_t *entry = instrlist_create(context);
        instr_t *branch = INSTR_CREATE_b(context, opnd_create_pc(shared));
        instrlist_meta_append(entry, branch);
        dr_insert_read_raw_tls(context, entry, branch, budget_segment, slot(SPILL),
                               candidates[i]);
        byte *start = dispatch_entry(candidates[i]);
        DR_ASSERT(instrlist_encode(context, entry, start, false) == start + ENTRY_BYTES);
        instrlist_clear_and_destroy(context, entry);
    }
    /* A callee that returns has stored a code cache entry: DR's jump to it
     * follows the clean call. It is built apart from the call for the same
     * reason as the entries. */
    instrlist_t *resume = instrlist_create(context);
    instr_t *resumed = INSTR_CREATE_label(context);
    instrlist_meta_append(resume, resumed);
    dr_insert_redirect_entry_jump(context, resume, resumed,
                                  dr_raw_tls_opnd(context, budget_segment, slot(ENTRY)));
    last = instrlist_encode(context, resume, last, false);
    DR_ASSERT(last != NULL && last <= dispatch_routine + dispatch_size);
    instrlist_clear_and_destroy(context, resume);
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
#ifdef AARCH64
    for (size_t i = 0; i < candidate_count; i++)
        *(byte **)(base + slot(DISPATCH + candidates[i] - DR_REG_X0)) =
            dispatch_entry(candidates[i]);
#else
    *(byte **)(base + slot(DISPATCH)) = dispatch_routine;
#endif
}

static uint scratch_index(reg_id_t reg)
{
#ifdef X86_64
    DR_ASSERT(reg == DR_REG_RCX);
    return 0;
#else
    return reg - DR_REG_X0;
#endif
}

static reg_id_t scratch_register(uint index)
{
#ifdef X86_64
    return DR_REG_RCX;
#else
    return DR_REG_X0 + index;
#endif
}

static void *scratch_note(uint kind, reg_id_t reg)
{
    return (void *)(scratch_notes + kind * NOTE_REGISTERS + scratch_index(reg));
}

/* NULL translations keep the charge instructions from being relocation points,
 * but the scratch window also covers application instructions when the scratch
 * register is dead: a fault there must report the application's value. Repair
 * it when DR reconstructs an instruction list; stored translation tables have
 * no list and remain PC-only inside the window (this client never asks DR to
 * store them, and DR's code copies are rebuilt as lists). */
static bool restore_scratch(void *context, bool restore_memory,
                            dr_restore_state_info_t *info)
{
    (void)restore_memory;
    byte *pc = info->fragment_info.cache_start_pc;
    if (info->fragment_info.ilist == NULL || pc == NULL || !info->raw_mcontext_valid)
        return true;
    reg_id_t held = DR_REG_NULL;
    bool spanning = false;
    for (instr_t *instruction = instrlist_first(info->fragment_info.ilist);
         instruction != NULL && pc <= info->raw_mcontext->pc;
         instruction = instr_get_next(instruction)) {
        int length = instr_length(context, instruction);
        if (length == 0)
            continue;
        ptr_uint_t note = (ptr_uint_t)instr_get_note(instruction) - scratch_notes;
        bool inside = spanning;
        bool last = false;
        if (note < NOTE_KINDS * NOTE_REGISTERS) {
            uint kind = note / NOTE_REGISTERS;
            reg_id_t reg = scratch_register(note % NOTE_REGISTERS);
            if (kind == LAST)
                last = spanning && reg == held;
            else {
                held = reg;
                inside = true;
                spanning = kind == SPANS;
            }
        }
        if (pc == info->raw_mcontext->pc) {
            if (inside) {
                byte *base = drmgr_get_tls_field(context, segment_field);
                reg_set_value(held, info->mcontext, *(reg_t *)(base + slot(SPILL)));
            }
            break;
        }
        if (last)
            spanning = false;
        pc += length;
    }
    return true;
}

#ifdef AARCH64
/* Bit n stands for Xn (or Wn). Other registers have no bit. */
static uint gpr_bit(reg_id_t reg)
{
    if (reg == DR_REG_NULL)
        return 0;
    reg = reg_to_pointer_sized(reg);
    return reg >= DR_REG_X0 && reg <= DR_REG_X30 ? 1u << (reg - DR_REG_X0) : 0;
}

/* The X registers an instruction reads and writes. Address registers of a
 * memory destination are reads; a predicated write also counts as a read. */
static void gpr_uses(instr_t *instruction, uint *reads, uint *writes)
{
    *reads = *writes = 0;
    for (int i = 0; i < instr_num_srcs(instruction); i++) {
        opnd_t operand = instr_get_src(instruction, i);
        for (int j = 0; j < opnd_num_regs_used(operand); j++)
            *reads |= gpr_bit(opnd_get_reg_used(operand, j));
    }
    for (int i = 0; i < instr_num_dsts(instruction); i++) {
        opnd_t operand = instr_get_dst(instruction, i);
        if (opnd_is_reg(operand))
            *writes |= gpr_bit(opnd_get_reg(operand));
        else {
            for (int j = 0; j < opnd_num_regs_used(operand); j++)
                *reads |= gpr_bit(opnd_get_reg_used(operand, j));
        }
    }
    if (instr_is_predicated(instruction))
        *reads |= *writes;
}

/* Whether DR keeps `instruction` as it is, so the register it writes holds the
 * application's value at every later PC and the instruction keeps its note.
 * The excluded kinds are those DR mangles. */
static bool plain_write(instr_t *instruction)
{
    int opcode = instr_get_opcode(instruction);
    return !instr_is_cti(instruction) && !instr_is_syscall(instruction) &&
        !instr_has_rel_addr_reference(instruction) &&
        !instr_is_interrupt(instruction) && !instr_is_exclusive_load(instruction) &&
        !instr_is_exclusive_store(instruction) &&
        !instr_uses_reg(instruction, dr_get_stolen_reg()) && opcode != OP_mrs &&
        opcode != OP_msr && opcode != OP_sys && opcode != OP_sysl && opcode != OP_ld64b;
}


/* Choose the block's scratch register. A register whose first use in the
 * block is a plain write is dead: it needs no restore, and its window ends
 * after that write (*overwrite). Otherwise *overwrite is NULL and the register
 * is restored before the first application instruction; prefer one the block
 * never names, then the one it reads last, so the reload is off the
 * application's critical path. An instruction DR cannot decode ends the scan:
 * registers it might use are not dead. */
static reg_id_t choose_scratch(instrlist_t *block, instr_t **overwrite)
{
    enum { UNNAMED = 0x7fffffff };
    uint seen = 0, dead = 0;
    int first_use[31];
    instr_t *write[31];
    for (int i = 0; i < 31; i++)
        first_use[i] = UNNAMED;
    int position = 0;
    for (instr_t *instruction = instrlist_first_app(block); instruction != NULL;
         instruction = instr_get_next_app(instruction), position++) {
        uint reads, writes;
        if (!instr_valid(instruction) || instr_get_opcode(instruction) == OP_xx)
            reads = ~0u, writes = 0;
        else
            gpr_uses(instruction, &reads, &writes);
        uint fresh = (reads | writes) & ~seen & 0x7fffffffu;
        for (uint bits = fresh; bits != 0; bits &= bits - 1) {
            int index = __builtin_ctz(bits);
            first_use[index] = position;
            write[index] = instruction;
        }
        if (plain_write(instruction))
            dead |= fresh & writes & ~reads;
        seen |= fresh;
        if (reads == ~0u)
            break;
    }
    *overwrite = NULL;
    for (size_t i = 0; i < candidate_count; i++) {
        int index = candidates[i] - DR_REG_X0;
        if (candidates[i] != dr_get_stolen_reg() && (dead & (1u << index))) {
            *overwrite = write[index];
            return candidates[i];
        }
    }
    reg_id_t latest = DR_REG_NULL;
    for (size_t i = 0; i < candidate_count; i++) {
        if (candidates[i] == dr_get_stolen_reg())
            continue;
        if (latest == DR_REG_NULL ||
            first_use[candidates[i] - DR_REG_X0] > first_use[latest - DR_REG_X0])
            latest = candidates[i];
        if (first_use[latest - DR_REG_X0] == UNNAMED)
            break;
    }
    return latest;
}
#endif

/* Insert the budget decrement and checkpoint call before `where`. */
static void charge_before(void *context, instrlist_t *block, instr_t *where)
{
    instr_t *overwrite = NULL;
#ifdef X86_64
    const reg_id_t SCRATCH = DR_REG_RCX;
#else
    const reg_id_t SCRATCH = choose_scratch(block, &overwrite);
#endif
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
    /* The first instruction may be an entropy label, which carries the
     * application PC of the instruction it precedes. */
    app_pc resume = instr_get_app_pc(instrlist_first_app(block));
    DR_ASSERT(resume != NULL);
    instrlist_insert_mov_immed_ptrsz(context, (ptr_int_t)resume, opnd_create_reg(SCRATCH),
                                     block, where, NULL, NULL);
    dr_insert_write_raw_tls(context, block, where, budget_segment, slot(RESUME), SCRATCH);
#ifdef X86_64
    instrlist_meta_preinsert(block, where, INSTR_CREATE_jmp_ind(context,
        dr_raw_tls_opnd(context, budget_segment, slot(DISPATCH))));
#else
    /* Branch to SCRATCH's own entry of the exhaustion routine. */
    dr_insert_read_raw_tls(context, block, where, budget_segment,
                           slot(DISPATCH + SCRATCH - DR_REG_X0), SCRATCH);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_br(context, opnd_create_reg(SCRATCH)));
#endif
    instrlist_meta_preinsert(block, where, skip);
    /* A dead register needs no restore: the application overwrites it. A NOP
     * keeps the instrumentation's length. */
    if (overwrite != NULL) {
        instrlist_meta_preinsert(block, where, INSTR_CREATE_nop(context));
        instr_set_note(overwrite, scratch_note(LAST, SCRATCH));
    } else
        dr_insert_read_raw_tls(context, block, where, budget_segment, slot(SPILL), SCRATCH);
    void *note = scratch_note(overwrite != NULL ? SPANS : HELD, SCRATCH);
    for (instr_t *held = instr_get_next(saved); held != where; held = instr_get_next(held))
        instr_set_note(held, note);
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
        /* The label keeps the original translation too: it is the block's
         * first application instruction when the entropy instruction began
         * the block, and the budget charge resumes from that PC. */
        instr_set_translation(label, instr_get_app_pc(instruction));
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
    scratch_notes = drmgr_reserve_note_range(NOTE_KINDS * NOTE_REGISTERS);
    DR_ASSERT(scratch_notes != DRMGR_NOTE_NONE);
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
