/* Automatic application progress, including shared libraries and JIT code.
 * Each thread charges one PV syscall per 10,000 counted block entries.
 * The budget is part of execution semantics, not a performance setting.
 *
 * On AArch64, host interruptions can make exclusive stores fail even with
 * one guest CPU. Counting their retries can change the budget and clock.
 * Packing supported sequences and skipping their load/store blocks reduces
 * this effect for simple retry loops. Unpacked polls that branch back before
 * a store are charged on that branch so they can advance virtual time.
 *
 * This is a partial mitigation, not a control-flow or determinism proof:
 * split sequences, retry paths outside these blocks, application-visible
 * failures and the translator's own atomics remain exposed. Packed polls
 * can also stall without a progress charge. See docs/progress.md for the
 * limitations and the independent exclusive-monitor follow-up.
 */
#include "dr_api.h"
#include "drmgr.h"
#include "drreg.h"
#include "drvector.h"

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

/* What a block charges: its entry, or the taken path of its last branch. */
enum { CHARGE_ENTRY = 1, CHARGE_ABANDON = 2 };

#ifdef AARCH64
/* Whether a packed sequence branches back to its load before its store: a
 * poll loop that spins inside the macro instruction while it waits, where
 * nothing can be charged. */
static bool packed_spins_before_store(void *context, instr_t *packed)
{
    app_pc start = instr_get_app_pc(packed), load = NULL;
    int length = instr_length(context, packed);
    bool spins = false;
    instr_t instr;
    instr_init(context, &instr);
    for (int offset = 0; offset < length; offset += 4) {
        instr_reset(context, &instr);
        if (decode(context, start + offset, &instr) == NULL ||
            instr_is_exclusive_store(&instr))
            break;
        if (load == NULL && instr_is_exclusive_load(&instr))
            load = start + offset;
        if (load != NULL && (instr_is_cbr(&instr) || instr_is_ubr(&instr)) &&
            instr_get_branch_target_pc(&instr) <= load) {
            spins = true;
            break;
        }
    }
    instr_free(context, &instr);
    return spins;
}

/* Whether a block that begins with an exclusive load ends, before any
 * exclusive store, with a branch back to the load: its taken path abandons
 * the reservation, and is where the poll is charged. */
static bool abandons_sequence(instrlist_t *block, instr_t *first)
{
    instr_t *last = instrlist_last_app(block);
    for (instr_t *i = first; i; i = instr_get_next_app(i)) {
        if (instr_is_exclusive_store(i))
            return false;
    }
    switch (instr_get_opcode(last)) {
    case OP_b:
    case OP_bcond:
    case OP_cbnz:
    case OP_cbz:
    case OP_tbnz:
    case OP_tbz:
        return instr_get_branch_target_pc(last) <= instr_get_app_pc(first);
    default:
        return false;
    }
}
#endif

/* Decide once per block what it charges. */
static dr_emit_flags_t analyze(void *context, void *tag, instrlist_t *block,
                               bool trace, bool translating, void **user)
{
    ptr_uint_t charge = CHARGE_ENTRY;
#ifdef AARCH64
    instr_t *first = instrlist_first_app(block);
    for (instr_t *i = first; i; i = instr_get_next_app(i)) {
        int opcode = instr_get_opcode(i);
        if (opcode == OP_ldstex) {
            /* First: the retry target of a packed sequence. Otherwise the
             * sequence is entered and retried inside this block, below the
             * entry charge. */
            if (i == first)
                charge = 0;
            if (packed_spins_before_store(context, i))
                dr_fprintf(STDERR, "seedbox progress: exclusive poll loop at %p cannot be "
                           "charged while it waits\n", instr_get_app_pc(i));
            break;
        }
        if (instr_is_exclusive_load(i)) {
            /* First: the retry target of a sequence DynamoRIO could not pack.
             * Otherwise ordinary instructions precede the load, the retry
             * jumps to the load, and DynamoRIO gives that target a block of
             * its own. */
            if (i == first)
                charge = abandons_sequence(block, first) ? CHARGE_ABANDON : 0;
            break;
        }
        if (instr_is_exclusive_store(i) || opcode == OP_clrex) {
            charge = 0; /* resumes a sequence split across blocks */
            break;
        }
    }
#endif
    *user = (void *)charge;
    return DR_EMIT_DEFAULT;
}

/* Insert the budget decrement and the checkpoint call before `where`. With
 * `branch`, the charge runs only when that conditional branch is taken. */
static void charge_before(void *context, instrlist_t *block, instr_t *where,
                          instr_t *branch)
{
    instr_t *skip = INSTR_CREATE_label(context);
#ifdef X86_64
    (void)branch;
    DR_ASSERT(drreg_reserve_aflags(context, block, where) == DRREG_SUCCESS);
    instrlist_meta_preinsert(block, where, INSTR_CREATE_sub(context,
        dr_raw_tls_opnd(context, budget_segment, budget_offset), OPND_CREATE_INT8(1)));
    instrlist_meta_preinsert(block, where,
        INSTR_CREATE_jcc(context, OP_jnz, opnd_create_instr(skip)));
#elif defined(AARCH64)
    /* Non-flag-setting arithmetic and CBNZ preserve application NZCV. The
     * registers the branch reads stay live for its meta copy below. */
    drvector_t allowed;
    drreg_init_and_fill_vector(&allowed, true);
    for (int s = 0; branch != NULL && s < instr_num_srcs(branch); ++s) {
        opnd_t source = instr_get_src(branch, s);
        if (opnd_is_reg(source))
            drreg_set_vector_entry(&allowed, reg_to_pointer_sized(opnd_get_reg(source)), false);
    }
    reg_id_t scratch;
    DR_ASSERT(drreg_reserve_register(context, block, where, &allowed, &scratch)
              == DRREG_SUCCESS);
    drvector_delete(&allowed);
    if (branch != NULL) {
        instr_t *inverted = instr_clone(context, branch);
        instr_set_meta(inverted);
        instr_invert_cbr(inverted);
        instr_set_target(inverted, opnd_create_instr(skip));
        instrlist_meta_preinsert(block, where, inverted);
    }
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

static dr_emit_flags_t instrument(void *context, void *tag, instrlist_t *block,
                                 instr_t *instruction, bool trace,
                                 bool translating, void *user)
{
    ptr_uint_t charge = (ptr_uint_t)user;
    if ((charge & CHARGE_ENTRY) && drmgr_is_first_instr(context, instruction))
        charge_before(context, block, instruction, NULL);
    else if ((charge & CHARGE_ABANDON) && drmgr_is_last_instr(context, instruction))
        charge_before(context, block, instruction,
                      instr_is_cbr(instruction) ? instruction : NULL);
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
    drmgr_register_bb_instrumentation_event(analyze, instrument, NULL);
    dr_register_exit_event(finished);
}
