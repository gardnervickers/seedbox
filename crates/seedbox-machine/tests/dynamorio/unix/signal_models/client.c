/* Public API adaptation of DR:a64_prop_signal's dr_itimer_cb (Google 2026).
 * See LICENSE. No app state, private headers, code-cache or TLS layout access. */
#include "dr_api.h"
#include <sys/time.h>
static int counts[3];
static void real_cb(void *dc, dr_mcontext_t *mc) { (void)dc; (void)mc; dr_atomic_add32_return_sum(&counts[0], 1); }
static void virtual_cb(void *dc, dr_mcontext_t *mc) { (void)dc; (void)mc; dr_atomic_add32_return_sum(&counts[1], 1); }
static void prof_cb(void *dc, dr_mcontext_t *mc) { (void)dc; (void)mc; dr_atomic_add32_return_sum(&counts[2], 1); }
static void finished(void)
{
    dr_fprintf(STDERR, "signal-model-client callbacks=%d,%d,%d\n", counts[0], counts[1], counts[2]);
    if (counts[0] <= 0 || counts[1] <= 0 || counts[2] <= 0) dr_abort_with_code(1);
}
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[])
{
    (void)id; (void)argc; (void)argv;
    dr_register_exit_event(finished);
    if (!dr_set_itimer(ITIMER_REAL, 3, real_cb) ||
        !dr_set_itimer(ITIMER_VIRTUAL, 4, virtual_cb) ||
        !dr_set_itimer(ITIMER_PROF, 5, prof_cb)) dr_abort_with_code(1);
}
