/* Automatic application progress, including shared libraries and JIT code.
 * Each thread charges one PV syscall before every 10,000th translated block.
 * The budget is part of execution semantics, not a performance setting.
 *
 * Why AArch64 atomics need special care
 * -------------------------------------
 *
 * ARM has no single atomic read-modify-write instruction in the code that
 * musl, glibc, Go and V8 ship. An atomic increment is a short sequence:
 *
 *     retry:  ldxr  x0, [x1]        ; load and arm the "exclusive monitor"
 *             add   x0, x0, #1
 *             stxr  w2, x0, [x1]     ; store only if the monitor is still armed
 *             cbnz  w2, retry        ; otherwise try again
 *
 * The monitor is disarmed by any exception return into the guest. On real
 * hardware that happens on interrupts and context switches; under a
 * hypervisor it also happens every time the host takes the CPU away and
 * gives it back, for example to service a host interrupt. The VMM never
 * learns about those entries (hv_vcpu_run only returns for guest exceptions,
 * cancellation and the virtual timer), so a run and its replay see the
 * monitor disarmed at different moments and the stxr fails a different
 * number of times in each.
 *
 * For a plain retry loop like the one above that is harmless: when the loop
 * finally succeeds the registers and memory look exactly as if the first
 * attempt had worked. (A program can observe a failure when it chooses to,
 * for example a weak compare-and-swap that counts attempts or backs off, and
 * such a program is as non-reproducible here as it is natively.)
 *
 * This client would make every retry visible. We count block entries, and
 * each retry jumps back to the block that starts the sequence. If that entry
 * were counted, a run with three retries would spend three more units of
 * budget than a replay with none. That shifts the point where the budget
 * reaches zero and the getpid charge is issued. Two consequences follow:
 *
 *   - the budget value itself lives in memory (raw TLS), so RAM differs
 *     between run and replay even if nothing else does;
 *   - once the shift crosses a 10,000 boundary, the progress charge moves to
 *     a different instruction, and the guest clock differs too.
 *
 * Splitting a sequence across several blocks is even worse: the first
 * execution of every block boundary inside the sequence is a translator
 * exit that disarms the monitor by itself, and any instrumentation inside
 * the sequence stores to memory while the monitor is armed, which an
 * implementation may treat as a reason to disarm it.
 *
 * The rule
 * --------
 *
 * Never count a block entry that a failed store-exclusive can reach, and
 * never place instructions between the load and the store. Concretely:
 *
 *   1. The supervisor runs DynamoRIO with -unsafe_build_ldstex, so each
 *      sequence is packed into one block as a single "ldstex" macro
 *      instruction, and with -no_ldstex2cas, because DynamoRIO's other
 *      option (rewriting the sequence into a compare-and-swap) changes ABA
 *      semantics and hangs static musl binaries.
 *   2. analyze() below marks a block as "not counted" when it begins with an
 *      exclusive load or a packed ldstex, since that block is the retry
 *      target. For the sequences DynamoRIO could not pack (more than
 *      sixteen instructions, a PC-relative instruction inside, or every
 *      scratch register in use) it also leaves uncounted any block that
 *      continues a sequence: one that contains a store-exclusive or clrex
 *      before any load, and one whose preceding instructions arm the
 *      monitor without releasing it. Instrumentation is never placed inside
 *      a sequence.
 *   3. The progress count lands on the block the sequence exits to. That
 *      block runs exactly once per completed atomic operation, however many
 *      retries preceded it, so the budget and the charge no longer depend on
 *      host timing.
 *   4. A sequence that gives up before its store, for instance a spinlock
 *      that reloads while the lock is held, re-enters the load without
 *      completing anything, so no exit block runs. When DynamoRIO could not
 *      pack it, the block begins with the load and ends with that backward
 *      branch, and the charge is placed on the branch's taken path: the
 *      reservation is abandoned there, so nothing can fail because of it,
 *      and the path runs once per poll, never because of a failed store.
 *      When DynamoRIO did pack it, the loop spins inside the macro
 *      instruction where nothing can be inserted; analyze() reports such a
 *      block on stderr once, because a waiter in it makes no progress until
 *      something else moves the clock.
 *
 * A block where ordinary instructions precede the exclusive load is still
 * counted: the retry jumps to the load, not to the block start, so DynamoRIO
 * gives the retry target a block of its own, and that one is not counted.
 *
 * The retry count itself is still not reproducible (a program that prints
 * its own stxr failure count differs between run and replay, as it does
 * natively). The rule only makes progress accounting independent of it.
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
/* DynamoRIO packs at most this many instructions into one ldstex block. */
#define SEQUENCE_WINDOW 16

/* Whether the instructions before `pc` arm the monitor without releasing it:
 * an exclusive load within the window with no exclusive store, clrex, system
 * call or indirect branch between it and `pc`. A block that starts there
 * continues a sequence DynamoRIO could not pack. */
static bool continues_sequence(void *context, app_pc pc)
{
    bool inside = false;
    instr_t instr;
    instr_init(context, &instr);
    for (int k = 1; k <= SEQUENCE_WINDOW; ++k) {
        app_pc at = pc - 4 * k;
        instr_reset(context, &instr);
        if (!dr_memory_is_readable(at, 4) || decode(context, at, &instr) == NULL)
            break;
        if (instr_is_exclusive_store(&instr) || instr_get_opcode(&instr) == OP_clrex ||
            instr_is_syscall(&instr) || instr_is_mbr(&instr))
            break;
        if (instr_is_exclusive_load(&instr)) {
            inside = true;
            break;
        }
    }
    instr_free(context, &instr);
    return inside;
}

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
    bool exclusive = false;
    for (instr_t *i = first; i; i = instr_get_next_app(i)) {
        int opcode = instr_get_opcode(i);
        if (opcode == OP_ldstex) {
            /* First: the retry target of a packed sequence. Otherwise the
             * sequence is entered and retried inside this block, below the
             * entry charge. */
            exclusive = true;
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
            exclusive = true;
            if (i == first)
                charge = abandons_sequence(block, first) ? CHARGE_ABANDON : 0;
            break;
        }
        if (instr_is_exclusive_store(i) || opcode == OP_clrex) {
            charge = 0; /* resumes a sequence split across blocks */
            exclusive = true;
            break;
        }
    }
    if (!exclusive && first != NULL && continues_sequence(context, instr_get_app_pc(first)))
        charge = 0; /* inside a split sequence, between its load and store */
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
