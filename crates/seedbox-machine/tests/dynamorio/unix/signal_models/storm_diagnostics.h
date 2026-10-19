/* Bounded application diagnostics only; never a success oracle.
 * 40 independent round records of native-endian lock-free uint64_t words.
 * A nonzero round is its publication tag (round index + 1). Roles atomically
 * pack TID in high 32 bits and storm.c source line in low 32 bits. controller_seq
 * is odd during predicate publication, even afterwards; bits 0..3 are sender,
 * notification, process timer, churn; bits 4..7 are waiter EINTR thresholds.
 * Readers take bounded samples, tolerate partial/mixed observations, and never
 * wait for a coherent snapshot. Mirrored counters can lead or lag app updates;
 * progress_posts counts attempts before sem_post, not successful returns.
 * wake_attempt is 1 before write and 2 after result/errno publication (a -1
 * result is encoded as UINT64_MAX). No handlers perform I/O or acquire locks.
 */
#include <fcntl.h>
#include <sys/mman.h>
#define STORM_DIAG_FIELDS(D) \
    D(round) \
    D(pid) \
    D(main_role) \
    D(controller_role) \
    D(sender_role) \
    D(churn_role) \
    D(poker_role) \
    D(controller_seq) \
    D(predicate) \
    D(sent) \
    D(acknowledged) \
    D(sender_done) \
    D(t3_acct) \
    D(churn_created) \
    D(churn_live) \
    D(churn_done) \
    D(round_over) \
    D(churn_on) \
    D(notifications) \
    D(inflight) \
    D(progress_posts) \
    D(w0_role) \
    D(w0_armed) \
    D(w0_eintr) \
    D(w0_wake_attempt) \
    D(w0_wake_result) \
    D(w0_wake_errno) \
    D(w1_role) \
    D(w1_armed) \
    D(w1_eintr) \
    D(w1_wake_attempt) \
    D(w1_wake_result) \
    D(w1_wake_errno) \
    D(w2_role) \
    D(w2_armed) \
    D(w2_eintr) \
    D(w2_wake_attempt) \
    D(w2_wake_result) \
    D(w2_wake_errno) \
    D(w3_role) \
    D(w3_armed) \
    D(w3_eintr) \
    D(w3_wake_attempt) \
    D(w3_wake_result) \
    D(w3_wake_errno)
#define DIAG_ENUM(name) D_##name,
enum { STORM_DIAG_FIELDS(DIAG_ENUM) D_COUNT };
#undef DIAG_ENUM
static _Atomic uint64_t (*diagnostics)[D_COUNT];
static _Atomic uint64_t *diag;
_Static_assert(ATOMIC_LLONG_LOCK_FREE == 2 && ATOMIC_LONG_LOCK_FREE == 2,
               "diagnostics require lock-free 64-bit atomics");
static void diag_init(void)
{
    int fd = open("storm-diagnostic.bin", O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    REQUIRE(fd >= 0);
    size_t size = 40 * D_COUNT * sizeof(_Atomic uint64_t);
    REQUIRE(ftruncate(fd, size) == 0);
    diagnostics = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    REQUIRE(diagnostics != MAP_FAILED);
    REQUIRE(close(fd) == 0);
}
static __thread uint64_t diagnostic_tid;
static uint64_t diag_role(unsigned line)
{
    if (!diagnostic_tid) diagnostic_tid = (uint64_t)syscall(SYS_gettid) << 32;
    return diagnostic_tid | line;
}
#define ROLE(name) (diag[D_##name##_role] = diag_role(__LINE__))
#define WROLE(w) (diag[D_w0_role + (w) * 6] = diag_role(__LINE__))
static int post_progress(void)
{
    atomic_fetch_add(&diag[D_progress_posts], 1);
    return sem_post(&progress);
}
