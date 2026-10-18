#define _GNU_SOURCE
#include "guest_supervisor.h"
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void close_outputs(struct sbx_supervisor *s) {
    for (unsigned i = 0; i < 2; ++i) {
        if (s->output[i] >= 0) close(s->output[i]);
        s->output[i] = -1;
    }
}
void sbx_supervisor_init(struct sbx_supervisor *s) {
    s->child = -1;
    s->output[0] = s->output[1] = -1;
    s->failure = SBX_FAILURE_NONE;
}
static int make_pipe(int fd[2]) {
    if (pipe(fd)) return -1;
    if (fcntl(fd[0], F_SETFD, FD_CLOEXEC) < 0 ||
        fcntl(fd[1], F_SETFD, FD_CLOEXEC) < 0) {
        int error = errno;
        close(fd[0]); close(fd[1]); errno = error;
        return -1;
    }
    return 0;
}
int sbx_supervisor_start(struct sbx_supervisor *s, sbx_launch launch, void *recipe) {
    s->failure = SBX_FAILURE_NONE;
    if (s->child > 0) { s->failure = SBX_FAILURE_REJECTED; errno = EBUSY; return -1; }
    int pipes[3][2] = {{-1, -1}, {-1, -1}, {-1, -1}};
    close_outputs(s);
    for (unsigned i = 0; i < 3; ++i) {
        if (make_pipe(pipes[i])) goto failed;
    }
#ifdef __linux__
    /* Linux rechecks pipe inode permissions when a non-root application
     * reopens /dev/stdout through /proc/self/fd. Preserve prepared image use. */
    for (unsigned i = 0; i < 2; ++i)
        if (fchmod(pipes[i][1], 0666)) goto failed;
#endif
    pid_t child = fork();
    if (child < 0) goto failed;
    if (!child) {
        close(pipes[2][0]);
        int error = 0;
        if (setpgid(0, 0)) error = errno;
        for (unsigned i = 0; i < 2; ++i) {
            close(pipes[i][0]);
            if (!error && dup2(pipes[i][1], (int)i + 1) < 0) error = errno;
            close(pipes[i][1]);
        }
        int input = open("/dev/null", O_RDONLY);
        if (input < 0) error = errno;
        else {
            if (dup2(input, STDIN_FILENO) < 0) error = errno;
            if (input > 2) close(input);
        }
        if (!error) error = launch(recipe, pipes[2][1]);
        if (!error) error = EIO;
        const unsigned char *bytes = (const unsigned char *)&error;
        size_t used = 0;
        while (used < sizeof(error)) {
            ssize_t count = write(pipes[2][1], bytes + used, sizeof(error) - used);
            if (count < 0 && errno == EINTR) continue;
            if (count <= 0) break;
            used += (size_t)count;
        }
        _exit(127);
    }
    for (unsigned i = 0; i < 3; ++i) {
        close(pipes[i][1]); pipes[i][1] = -1;
    }
    int launch_error = 0;
    size_t received = 0;
    int reported = 1;
    while (received < sizeof(launch_error)) {
        ssize_t count = read(pipes[2][0], (unsigned char *)&launch_error + received,
                             sizeof(launch_error) - received);
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) { reported = 0; launch_error = errno; received = sizeof(launch_error); break; }
        if (!count) break;
        received += (size_t)count;
    }
    if (received) {
        if (received != sizeof(launch_error) || !launch_error) { reported = 0; launch_error = EIO; }
        kill(-child, SIGKILL);
        kill(child, SIGKILL);
        while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
        errno = launch_error;
        s->failure = reported ? SBX_FAILURE_REJECTED : SBX_FAILURE_FATAL;
        goto failed;
    }
    close(pipes[2][0]); pipes[2][0] = -1;
    for (unsigned i = 0; i < 2; ++i) {
        s->output[i] = pipes[i][0]; pipes[i][0] = -1;
        int flags = fcntl(s->output[i], F_GETFL);
        if (flags < 0 || fcntl(s->output[i], F_SETFL, flags | O_NONBLOCK) < 0) {
            int error = errno;
            kill(-child, SIGKILL); kill(child, SIGKILL);
            while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
            close_outputs(s); errno = error; goto failed;
        }
    }
    s->child = child;
    return 0;
failed: {
        if (s->failure == SBX_FAILURE_NONE) s->failure = SBX_FAILURE_FATAL;
        int error = errno;
        for (unsigned i = 0; i < 3; ++i)
            for (unsigned j = 0; j < 2; ++j)
                if (pipes[i][j] >= 0) close(pipes[i][j]);
        errno = error; return -1;
    }
}
static int read_output(struct sbx_supervisor *s, unsigned stream, size_t limit,
                       sbx_output output, void *sink) {
    unsigned char bytes[2048];
    if (limit > sizeof(bytes)) limit = sizeof(bytes);
    ssize_t count;
    do { count = read(s->output[stream], bytes, limit); } while (count < 0 && errno == EINTR);
    if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
    if (count < 0) return -1;
    if (!count) { close(s->output[stream]); s->output[stream] = -1; return 0; }
    if (output(sink, stream, bytes, (size_t)count)) return -1;
    return (int)count;
}
int sbx_supervisor_output(struct sbx_supervisor *s, unsigned stream,
                          sbx_output output, void *sink) {
    s->failure = SBX_FAILURE_NONE;
    if (stream > 1) { s->failure = SBX_FAILURE_FATAL; errno = EINVAL; return -1; }
    if (s->output[stream] < 0) return 0;
    if (read_output(s, stream, 2048, output, sink) < 0) { s->failure = SBX_FAILURE_FATAL; return -1; }
    return 0;
}
static int drain(struct sbx_supervisor *s, sbx_output output, void *sink) {
    for (unsigned stream = 0; stream < 2; ++stream) {
        if (s->output[stream] < 0) continue;
        int pending = 0;
        if (ioctl(s->output[stream], FIONREAD, &pending)) return -1;
        while (pending > 0) {
            int count = read_output(s, stream, (size_t)pending, output, sink);
            if (count < 0) return -1;
            if (!count) break;
            pending -= count;
        }
    }
    return 0;
}
int sbx_supervisor_stop(struct sbx_supervisor *s, sbx_output output, void *sink) {
    s->failure = SBX_FAILURE_REJECTED;
    if (s->child <= 0) { errno = EINVAL; return -1; }
    pid_t child = s->child;
    if (kill(-child, SIGKILL) && errno != ESRCH) return -1;
    while (waitpid(child, NULL, 0) < 0) { if (errno != EINTR) return -1; }
    s->child = -1;
    /* PID 1 (or a Linux subreaper) adopts the group's killed descendants. */
    while (waitpid(-child, NULL, 0) >= 0 || errno == EINTR) {}
    if (errno != ECHILD) return -1;
    int result = drain(s, output, sink), error = errno;
    s->failure = result ? SBX_FAILURE_FATAL : SBX_FAILURE_NONE;
    close_outputs(s); errno = error; return result;
}
int sbx_supervisor_pause(struct sbx_supervisor *s) {
    s->failure = SBX_FAILURE_REJECTED;
    if (s->child <= 0) { errno = EINVAL; return -1; }
    if (kill(-s->child, SIGSTOP)) return -1;
    siginfo_t info = {0};
    while (waitid(P_PID, (id_t)s->child, &info, WSTOPPED | WEXITED | WNOWAIT) < 0)
        if (errno != EINTR) return -1;
    if (info.si_code != CLD_STOPPED) { errno = ESRCH; return -1; }
    s->failure = SBX_FAILURE_NONE;
    return 0;
}
int sbx_supervisor_resume(struct sbx_supervisor *s) {
    s->failure = SBX_FAILURE_REJECTED;
    if (s->child <= 0) { errno = EINVAL; return -1; }
    int result = kill(-s->child, SIGCONT);
    if (!result) s->failure = SBX_FAILURE_NONE;
    return result;
}
int sbx_supervisor_collect(struct sbx_supervisor *s, int *exit_code,
                           sbx_output output, void *sink) {
    s->failure = SBX_FAILURE_NONE;
    int status, leader_status = 0, completed = 0;
    pid_t group = s->child;
    for (;;) {
        pid_t child = waitpid(-1, &status, WNOHANG);
        if (child < 0 && errno == EINTR) continue;
        if (child < 0 && errno != ECHILD) { s->failure = SBX_FAILURE_FATAL; return -1; }
        if (child <= 0) break;
        if (child == s->child) {
            leader_status = status;
            completed = 1;
            s->child = -1;
        }
    }
    if (!completed) return 0;
    /* The leader ending completes this application process group. Previously
     * guest poweroff ended its descendants; with a retained supervisor we must
     * finish that same group before permitting a nonoverlapping Start. Escaped
     * sessions and unrelated guest processes are outside this group boundary. */
    if (kill(-group, SIGKILL) && errno != ESRCH) {
        s->failure = SBX_FAILURE_FATAL; return -1;
    }
    while (waitpid(-group, NULL, 0) >= 0 || errno == EINTR) {}
    if (errno != ECHILD) { s->failure = SBX_FAILURE_FATAL; return -1; }
    *exit_code = WIFEXITED(leader_status) ? WEXITSTATUS(leader_status) : 128 + WTERMSIG(leader_status);
    int result = drain(s, output, sink), error = errno;
    if (result) s->failure = SBX_FAILURE_FATAL;
    close_outputs(s); errno = error;
    return result < 0 ? -1 : 1;
}
