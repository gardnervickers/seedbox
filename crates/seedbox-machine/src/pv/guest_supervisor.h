/* Private guest process mechanism, shared by the ARM64 and x86-64 init helpers.
 * This is not the host inbox/outbox contract or a management wire protocol. */
#ifndef SBX_GUEST_SUPERVISOR_H
#define SBX_GUEST_SUPERVISOR_H
#include <stddef.h>
#include <sys/types.h>

struct sbx_supervisor {
    pid_t child;
    int output[2];
    /* Command rejection can be acknowledged; mechanism/I/O failure must end
     * supervision, because continuing could conceal lost application bytes. */
    enum { SBX_FAILURE_NONE, SBX_FAILURE_REJECTED, SBX_FAILURE_FATAL } failure;
};

/* The retained recipe callback runs after fork with stdin=/dev/null and fresh
 * stdout/stderr pipes. It must exec, or return an errno value on setup failure.
 * Keep the supplied error_fd CLOEXEC and report any pre-exec child failure by
 * returning; never exit successfully before exec. Close inherited transport
 * descriptors and restore the application's signal mask here: a race-free
 * supervisor loop may have blocked SIGCHLD in the parent. Recipe storage
 * remains owned by the guest init process. */
typedef int (*sbx_launch)(void *recipe, int error_fd);
/* stream 0 is stdout; stream 1 is stderr. Bytes may include NUL and no newline.
 * Return zero after consuming all bytes, or -1 with errno on transport failure. */
typedef int (*sbx_output)(void *sink, unsigned stream,
                          const unsigned char *bytes, size_t length);

void sbx_supervisor_init(struct sbx_supervisor *s);
/* Zero means exec succeeded, not application-level readiness. A failed initial
 * launch and a failed later Start return the same errno; their caller owns the
 * distinct setup-failure vs command-ack behavior. Start requires no live child. */
int sbx_supervisor_start(struct sbx_supervisor *s, sbx_launch launch, void *recipe);
/* Stop kills the process group, reaps the leader and adopted descendants,
 * drains queued output, and closes pipes. It never emits a natural outcome. */
int sbx_supervisor_stop(struct sbx_supervisor *s, sbx_output output, void *sink);
/* Pause completes only after observing the leader stopped; it leaves an exit
 * pending for collect. Resume completes once SIGCONT was dispatched. */
int sbx_supervisor_pause(struct sbx_supervisor *s);
int sbx_supervisor_resume(struct sbx_supervisor *s);
/* Read one currently available chunk, without waiting for more output. */
int sbx_supervisor_output(struct sbx_supervisor *s, unsigned stream,
                          sbx_output output, void *sink);
/* 0: still running/no child; 1: natural exit collected and queued bytes drained;
 * -1: failure. Exit code is normal status or 128 + terminating signal. Does not
 * wait for descendant output EOF: remaining members of the same process group
 * are killed/reaped before final drain. Escaped sessions are not covered. The caller
 * must own child reaping (PID 1/subreaper): this also reaps adopted orphans
 * while the application leader remains alive. */
int sbx_supervisor_collect(struct sbx_supervisor *s, int *exit_code,
                           sbx_output output, void *sink);
#endif
