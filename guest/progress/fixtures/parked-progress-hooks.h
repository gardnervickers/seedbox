/* Test-only hooks compiled into the REAL progress client. Production builds
 * have neither budget injection nor the publisher-released post-SVC hold. */
#include "parked-publication-context.h"
static const char *parked_progress_target_path;
static int parked_progress_failure;
static dr_mcontext_t parked_progress_failure_context;
static byte *parked_progress_target;
static volatile int *parked_progress_req, *parked_progress_ack;
static struct parked_progress_diagnostic *parked_progress_diag;
static ptr_int_t *parked_progress_budget;
static uintptr_t parked_progress_owner_tls;
static uintptr_t parked_progress_tls(void)
{
    uintptr_t tls;
    __asm__ volatile("mrs %0, tpidr_el0" : "=r"(tls));
    return tls;
}

static ptr_int_t parked_progress_futex(volatile int *word, long op, int value)
{
    register long x0 __asm__("x0") = (long)word;
    register long x1 __asm__("x1") = op;
    register long x2 __asm__("x2") = value;
    register long x3 __asm__("x3") = 0;
    register long x4 __asm__("x4") = 0;
    register long x5 __asm__("x5") = 0;
    register long x8 __asm__("x8") = 98;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x4),
                     "r"(x5), "r"(x8) : "memory", "cc");
    return x0;
}

static uintptr_t parked_progress_hex(const char **p)
{
    uintptr_t value = 0;
    while (**p == ' ') ++*p;
    for (;;) {
        char c = **p;
        if (c >= '0' && c <= '9') value = value * 16 + c - '0';
        else if (c >= 'a' && c <= 'f') value = value * 16 + c - 'a' + 10;
        else break;
        ++*p;
    }
    return value;
}

static void parked_progress_load(void)
{
    char data[256];
    file_t file = dr_open_file(parked_progress_target_path, DR_FILE_READ);
    if (file == INVALID_FILE) return;
    ssize_t size = dr_read_file(file, data, sizeof(data) - 1);
    dr_close_file(file);
    if (size <= 0) return;
    data[size] = 0;
    const char *p = data;
    parked_progress_target = (byte *)parked_progress_hex(&p);
    parked_progress_req = (volatile int *)parked_progress_hex(&p);
    parked_progress_ack = (volatile int *)parked_progress_hex(&p);
    parked_progress_diag = (void *)parked_progress_hex(&p);
    DR_ASSERT(parked_progress_target && parked_progress_req && parked_progress_ack &&
              parked_progress_diag);
}

static void parked_progress_seed(uint instructions)
{
    struct parked_progress_diagnostic *d = parked_progress_diag;
    if (__atomic_load_n(parked_progress_req, __ATOMIC_ACQUIRE) == 0) {
        d->fresh_charge = instructions;
        return;
    }
    DR_ASSERT(d->armed == 0); /* A restarted paid helper must fail, not pay again. */
    parked_progress_budget = budget();
    __atomic_store_n(&parked_progress_owner_tls, parked_progress_tls(), __ATOMIC_RELEASE);
    if (parked_progress_failure == 2) {
        parked_progress_failure_context.size = sizeof(parked_progress_failure_context);
        parked_progress_failure_context.flags = DR_MC_ALL;
        DR_ASSERT(dr_get_mcontext(dr_get_current_drcontext(), &parked_progress_failure_context));
        parked_progress_failure_context.pc = parked_progress_target;
    }
    d->old_charge = instructions;
    d->initial_budget = d->iteration % 2 ? 2 : 2 - 2 * instruction_budget;
    if (parked_progress_failure == 4) d->initial_budget = instructions;
    d->armed = 1;
    *budget() = d->initial_budget;
}

static void parked_progress_instrument(void *context, instrlist_t *bb, instr_t *where,
                                      uint instructions)
{
    if (parked_progress_target == NULL) parked_progress_load();
    if (instr_get_app_pc(where) == parked_progress_target)
        dr_insert_clean_call(context, bb, where, (void *)parked_progress_seed, false, 1,
                             OPND_CREATE_INT32(instructions));
}

static void parked_progress_zero(void)
{
    parked_progress_diag->zero_budget = *budget();
    parked_progress_diag->armed = 0;
    __atomic_store_n(parked_progress_req, 0, __ATOMIC_RELEASE);
}

static void parked_progress_instrument_zero(void *context, instrlist_t *bb, instr_t *where)
{
    if (parked_progress_failure == 4 && instr_get_app_pc(where) == parked_progress_target)
        dr_insert_clean_call(context, bb, where, (void *)parked_progress_zero, false, 0);
}

static ptr_int_t parked_progress_after_payment(ptr_int_t result)
{
    struct parked_progress_diagnostic *d = parked_progress_diag;
    if (d == NULL || parked_progress_tls() !=
        __atomic_load_n(&parked_progress_owner_tls, __ATOMIC_ACQUIRE) || !d->armed) return result;
    if (parked_progress_failure == 1) return 0; /* Deliberate failure injection. */
    if (parked_progress_failure == 2)
        dr_app_progress_exit(&parked_progress_failure_context, parked_progress_budget,
                             instruction_budget, d->old_charge, NULL);
    if (parked_progress_failure == 3)
        *(volatile int *)(uintptr_t)1 = 0; /* Must be a DR/client fault, not app frame. */
    d->result = result; /* Actual completed marked getpid result, not reconstructed. */
    ++d->payments;
    if (d->payments != 1) return result;
    /* Read the actual committed add from the TLS address captured before entry. */
    d->after_add = *parked_progress_budget;
    __atomic_store_n(parked_progress_ack, 1, __ATOMIC_RELEASE);
    parked_progress_futex(parked_progress_ack, 129, 1);
    while (__atomic_load_n(parked_progress_req, __ATOMIC_ACQUIRE) == 1)
        parked_progress_futex(parked_progress_req, 128, 1);
    d->released = 1;
    return result;
}

static dr_signal_action_t parked_progress_signal(void *context, dr_siginfo_t *info)
{
    (void)context;
    if (info->sig == 10 && parked_progress_diag != NULL && parked_progress_diag->armed) {
        parked_progress_diag->before_signal = *budget();
        parked_progress_diag->deferred = parked_progress_diag->released;
        parked_progress_diag->armed = 0;
    }
    return DR_SIGNAL_DELIVER;
}
