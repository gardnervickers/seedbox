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
 * Natively that is harmless. The loop just retries, and when it finally
 * succeeds the registers and memory look exactly as if the first attempt
 * had worked. The program cannot tell how many retries happened.
 *
 * This client would make it visible. We count block entries, and each retry
 * jumps back to the block that starts the sequence. If that entry were
 * counted, a run with three retries would spend three more units of budget
 * than a replay with none. That shifts the point where the budget reaches
 * zero and the getpid charge is issued. Two consequences follow:
 *
 *   - the budget value itself lives in memory (raw TLS), so RAM differs
 *     between run and replay even if nothing else does;
 *   - once the shift crosses a 10,000 boundary, the progress charge moves to
 *     a different instruction, and the guest clock differs too.
 *
 * Splitting a sequence across several blocks is even worse: every block
 * boundary inside the sequence is a translator exit that disarms the monitor
 * by itself, deterministically, so even an uninterrupted run fails and
 * retries many times.
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
 *      target. It also marks a block that continues a sequence (one that
 *      contains a store-exclusive or clrex before any load) as not counted,
 *      for the cases DynamoRIO could not pack. Instrumentation is never
 *      placed inside a sequence.
 *   3. The progress count lands on the block the sequence exits to. That
 *      block runs exactly once per completed atomic operation, however many
 *      retries preceded it, so the budget and the charge no longer depend on
 *      host timing.
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

/* Decide once per block whether its entry is counted. */
static dr_emit_flags_t analyze(void *context, void *tag, instrlist_t *block,
                               bool trace, bool translating, void **user)
{
    bool counted = true;
#ifdef AARCH64
    for (instr_t *i = instrlist_first_app(block); i; i = instr_get_next_app(i)) {
        int opcode = instr_get_opcode(i);
        if (instr_is_exclusive_load(i) || opcode == OP_ldstex) {
            /* A sequence starts here: a failed store-exclusive re-enters this
             * block, unless ordinary instructions precede the load, in which
             * case the retry target is a block of its own. */
            counted = i != instrlist_first_app(block);
            break;
        }
        if (instr_is_exclusive_store(i) || opcode == OP_clrex) {
            counted = false; /* resumes a sequence split across blocks */
            break;
        }
    }
#endif
    *user = (void *)(ptr_uint_t)counted;
    return DR_EMIT_DEFAULT;
}

static dr_emit_flags_t instrument(void *context, void *tag, instrlist_t *block,
                                 instr_t *instruction, bool trace,
                                 bool translating, void *user)
{
    if (!drmgr_is_first_instr(context, instruction) || !(ptr_uint_t)user)
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
    drmgr_register_bb_instrumentation_event(analyze, instrument, NULL);
    dr_register_exit_event(finished);
}
