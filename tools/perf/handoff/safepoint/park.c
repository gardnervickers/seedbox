/* Scratch DynamoRIO client for the reproducer's "call" mode. It inserts a
 * clean call at the entry of the target block (the repro's JIT block on code
 * heap page 0). When the app has requested a park (park_req != 0), the clean
 * call reports itself parked (park_ack = park_req) and blocks in a futex wait
 * until the compiler thread clears park_req, so the flush and poll-page arm
 * happen while the thread is blocked in a syscall inside DR's clean call -
 * exactly where Kafka's main thread was parked (progress client's getpid).
 *
 * The block and handshake addresses are read lazily from
 * /tmp/repro-park-target, written by the repro before it first executes the
 * block. Built with DynamoRIO_USE_LIBC OFF; no extensions, so it loads against
 * both the stock and the Seedbox-patched DynamoRIO core.
 */
#include "dr_api.h"

#include <stdint.h>

static uint8_t *target_block;
static uint32_t *park_req, *park_ack;
static bool loaded;

#define NR_FUTEX 98
#define FUTEX_WAIT_PRIVATE_OP 128 /* FUTEX_WAIT | FUTEX_PRIVATE_FLAG */
#define FUTEX_WAKE_PRIVATE_OP 129 /* FUTEX_WAKE | FUTEX_PRIVATE_FLAG */

static long
raw_futex(uint32_t *uaddr, long op, uint32_t val)
{
#ifdef AARCH64
    register long number __asm__("x8") = NR_FUTEX;
    register long result __asm__("x0") = (long)uaddr;
    register long x1 __asm__("x1") = op;
    register long x2 __asm__("x2") = val;
    register long x3 __asm__("x3") = 0;
    register long x4 __asm__("x4") = 0;
    register long x5 __asm__("x5") = 0;
    __asm__ volatile("svc #0"
                     : "+r"(result)
                     : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(number)
                     : "memory", "cc");
    return result;
#else
    return -1;
#endif
}

static int do_redirect;

static void
park_call(void)
{
    uint32_t r;
    if (park_req == NULL)
        return;
    r = __atomic_load_n(park_req, __ATOMIC_ACQUIRE);
    if (r == 0)
        return;
    __atomic_store_n(park_ack, r, __ATOMIC_RELEASE);
    raw_futex(park_ack, FUTEX_WAKE_PRIVATE_OP, 1);
    while ((r = __atomic_load_n(park_req, __ATOMIC_ACQUIRE)) != 0)
        raw_futex(park_req, FUTEX_WAIT_PRIVATE_OP, r);
#ifdef AARCH64
    if (do_redirect) {
        /* PR #309 style: never return into the (possibly flushed) cached
         * fragment; leave through the dispatcher at the block's app pc. */
        dr_mcontext_t context = {sizeof(context), DR_MC_ALL};
        if (!dr_get_mcontext(dr_get_current_drcontext(), &context))
            dr_abort();
        context.pc = target_block;
        dr_redirect_execution(&context);
        dr_abort();
    }
#endif
}

static uint64_t
parse_hex(const char **p)
{
    uint64_t v = 0;
    while (**p == ' ')
        (*p)++;
    for (;;) {
        char c = **p;
        uint d;
        if (c >= '0' && c <= '9')
            d = (uint)(c - '0');
        else if (c >= 'a' && c <= 'f')
            d = (uint)(c - 'a' + 10);
        else
            break;
        v = v * 16 + d;
        (*p)++;
    }
    return v;
}

static void
load_target(void *drcontext)
{
    char buf[128];
    ssize_t got;
    const char *p = buf;
    file_t f = dr_open_file("/tmp/repro-park-target", DR_FILE_READ);
    if (f == INVALID_FILE)
        return;
    got = dr_read_file(f, buf, sizeof(buf) - 1);
    dr_close_file(f);
    if (got <= 0)
        return;
    buf[got] = 0;
    target_block = (uint8_t *)(uintptr_t)parse_hex(&p);
    park_req = (uint32_t *)(uintptr_t)parse_hex(&p);
    park_ack = (uint32_t *)(uintptr_t)parse_hex(&p);
    if (target_block == NULL || park_req == NULL || park_ack == NULL) {
        dr_fprintf(STDERR, "repro park: bad target file: %s\n", buf);
        dr_abort();
    }
    loaded = true;
    dr_fprintf(STDERR, "repro park: target=%p req=%p ack=%p\n", target_block, park_req,
               park_ack);
}

static dr_emit_flags_t
bb_event(void *drcontext, void *tag, instrlist_t *bb, bool for_trace, bool translating)
{
    (void)for_trace;
    (void)translating;
    if (!loaded)
        load_target(drcontext);
    if (loaded && tag == (void *)target_block) {
        if (do_redirect)
            dr_insert_clean_call_ex(drcontext, bb, instrlist_first(bb),
                                    (void *)park_call,
                                    DR_CLEANCALL_READS_APP_CONTEXT |
                                        DR_CLEANCALL_MULTIPATH,
                                    0);
        else
            dr_insert_clean_call(drcontext, bb, instrlist_first(bb), (void *)park_call,
                                 false, 0);
    }
    return DR_EMIT_DEFAULT;
}

DR_EXPORT void
dr_client_main(client_id_t id, int argc, const char *argv[])
{
    (void)id;
    for (int i = 0; i < argc; ++i)
        if (argv[i][0] == 'r') /* "redirect" */
            do_redirect = 1;
    dr_register_bb_event(bb_event);
}
