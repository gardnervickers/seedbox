/* Public signal actions and an exported application acknowledgement only. */
#include "dr_api.h"
#include <signal.h>
#include <sys/syscall.h>
static volatile int *receipts;
static int suppressed, ignored, defaulted, bypassed;
static dr_signal_action_t signal_event(void *context, dr_siginfo_t *info) {
    (void)context;
    if (info->sig != SIGUSR1 && info->sig != SIGSEGV &&
        info->sig != SIGWINCH && info->sig != SIGURG) return DR_SIGNAL_DELIVER;
    DR_ASSERT(!info->blocked);
    __atomic_add_fetch(receipts, 1, __ATOMIC_RELEASE);
    if (info->sig == SIGUSR1) {
        suppressed++;
        return DR_SIGNAL_SUPPRESS;
    }
    if (info->sig == SIGURG) {
        bypassed++;
        return DR_SIGNAL_BYPASS;
    }
    /* DELIVER edits apply only when the application has a handler. They must
     * not replace either the real read result or a never-issued continuation. */
#ifdef AARCH64
    info->mcontext->r0 = (reg_t)-123;
#else
    info->mcontext->xax = (reg_t)-123;
#endif
    if (info->sig == SIGSEGV) ignored++;
    else defaulted++;
    return DR_SIGNAL_DELIVER;
}
/* Public syscall observation keeps the raw read on the ordinary event path.
 * It does not change its arguments/result or force any signal timing. */
static bool filter_syscall(void *context, int number) {
    (void)context;
    return number == SYS_read;
}
static bool pre_syscall(void *context, int number) {
    (void)context; (void)number;
    return true;
}
static void finished(void) {
    DR_ASSERT(suppressed + ignored + defaulted + bypassed == 256);
    dr_fprintf(STDERR, "disposition-client: suppressed=%d ignored=%d defaulted=%d bypassed=%d\n",
               suppressed, ignored, defaulted, bypassed);
}
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[]) {
    (void)id; (void)argc; (void)argv;
    module_data_t *main = dr_get_main_module();
    DR_ASSERT(main != NULL);
    receipts = (volatile int *)dr_get_proc_address(main->handle, "disposition_receipts");
    DR_ASSERT(receipts != NULL);
    dr_free_module_data(main);
    dr_register_filter_syscall_event(filter_syscall);
    dr_register_pre_syscall_event(pre_syscall);
    dr_register_signal_event(signal_event);
    dr_register_exit_event(finished);
}
