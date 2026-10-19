/* Seedbox-owned external-test controller. No DR private APIs or root escalation.
 * The controller stays outside both the PID namespace and limited cgroup. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/types.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CGROUP2_MAGIC 0x63677270
static char leaf[PATH_MAX];
static pid_t owned = -1;

static int write_file(const char *path, const char *value)
{
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    size_t length = strlen(value);
    ssize_t n = write(fd, value, length);
    int saved = errno;
    close(fd);
    errno = saved;
    return n == (ssize_t)length ? 0 : -1;
}

static int member_path(char *out, const char *directory, const char *name)
{
    int n = snprintf(out, PATH_MAX, "%s/%s", directory, name);
    if (n < 0 || n >= PATH_MAX) { errno = ENAMETOOLONG; return -1; }
    return 0;
}

static int leaf_write(const char *name, const char *value)
{
    char path[PATH_MAX];
    return member_path(path, leaf, name) < 0 ? -1 : write_file(path, value);
}

static int cleanup(void)
{
    int failed = 0;
    if (leaf[0]) {
        /* cgroup.kill includes descendants which changed process groups. */
        if (leaf_write("cgroup.kill", "1") < 0) {
            fprintf(stderr, "isolate: cleanup cgroup.kill: %s\n", strerror(errno));
            failed = 1;
        }
    }
    if (owned > 0) {
        kill(-owned, SIGKILL);
        kill(owned, SIGKILL);
    }
    while (waitpid(-1, NULL, 0) > 0 || errno == EINTR) {}
    if (leaf[0] && rmdir(leaf) < 0) {
        fprintf(stderr, "isolate: cleanup %s: %s\n", leaf, strerror(errno));
        failed = 1;
    }
    leaf[0] = 0;
    owned = -1;
    return failed ? -1 : 0;
}

static int setup_error(const char *what)
{
    fprintf(stderr, "isolate: setup %s: %s\n", what, strerror(errno));
    cleanup();
    return 2;
}

static long long milliseconds(void)
{
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return -1;
    return (long long)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

static int new_pid_namespace(void)
{
    uid_t uid = getuid();
    gid_t gid = getgid();
    char map[80];
    if (unshare(CLONE_NEWUSER) < 0) return -1;
    if (write_file("/proc/self/setgroups", "deny") < 0) return -1;
    snprintf(map, sizeof(map), "0 %lu 1\n", (unsigned long)uid);
    if (write_file("/proc/self/uid_map", map) < 0) return -1;
    snprintf(map, sizeof(map), "0 %lu 1\n", (unsigned long)gid);
    if (write_file("/proc/self/gid_map", map) < 0) return -1;
    /* Only the next child enters the new PID namespace. There is no init
     * shim: that child execs drrun or the native application directly. */
    return unshare(CLONE_NEWPID);
}

static unsigned long long limit_hits(void)
{
    char path[PATH_MAX], key[80];
    unsigned long long value;
    if (member_path(path, leaf, "pids.events") < 0) return ULLONG_MAX;
    FILE *f = fopen(path, "r");
    if (!f) return ULLONG_MAX;
    while (fscanf(f, "%79s %llu", key, &value) == 2) {
        if (!strcmp(key, "max")) { fclose(f); return value; }
    }
    fclose(f);
    errno = EINVAL;
    return ULLONG_MAX;
}

static int new_pids_leaf(unsigned limit)
{
    const char *root = getenv("SEEDBOX_DYNAMORIO_CGROUP_ROOT");
    char resolved[PATH_MAX], path[PATH_MAX], value[80];
    struct statfs fs;
    struct stat st;
    if (!root || !*root) { errno = ENOENT; return -1; }
    if (!realpath(root, resolved) || statfs(resolved, &fs) < 0 ||
        stat(resolved, &st) < 0) return -1;
    if (fs.f_type != CGROUP2_MAGIC || st.st_uid != getuid()) {
        errno = EPERM; return -1;
    }
    /* The caller delegates an EMPTY, owned domain with pids already enabled.
     * Never change controllers or move tasks in an existing/shared group. */
    if (member_path(path, resolved, "cgroup.procs") < 0) return -1;
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    int c = fgetc(f);
    fclose(f);
    if (c != EOF) { errno = EBUSY; return -1; }
    unsigned long nonce;
    int random = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
    if (random < 0) return -1;
    ssize_t n = read(random, &nonce, sizeof(nonce));
    close(random);
    if (n != sizeof(nonce)) { errno = EIO; return -1; }
    int size = snprintf(path, sizeof(path), "%s/run-%ld-%lx", resolved,
                        (long)getpid(), nonce);
    if (size < 0 || size >= (int)sizeof(path)) { errno = ENAMETOOLONG; return -1; }
    if (mkdir(path, 0700) < 0) return -1;
    strcpy(leaf, path); /* only after successful exclusive ownership */
    snprintf(value, sizeof(value), "%u", limit);
    if (leaf_write("pids.max", value) < 0) return -1;
    if (member_path(path, leaf, "cgroup.kill") < 0 || access(path, W_OK)) return -1;
    return 0;
}

static int wait_owned(int signals)
{
    long long deadline = milliseconds() + 120000;
    for (;;) {
        siginfo_t info = {0};
        int done = waitid(P_PID, (id_t)owned, &info, WEXITED | WNOHANG | WNOWAIT);
        if (done == 0 && info.si_pid == owned) return 0;
        if (done < 0 && errno != EINTR) return -1;
        long long remaining = deadline - milliseconds();
        if (remaining <= 0) { errno = ETIMEDOUT; return -1; }
        struct pollfd p = { .fd = signals, .events = POLLIN };
        int ready = poll(&p, 1, (int)remaining);
        if (ready < 0 && errno == EINTR) continue;
        if (ready <= 0) { if (!ready) errno = ETIMEDOUT; return -1; }
        struct signalfd_siginfo event;
        if (read(signals, &event, sizeof(event)) != sizeof(event)) return -1;
        if (event.ssi_signo != SIGCHLD) { errno = ECANCELED; return -1; }
    }
}

int main(int argc, char **argv)
{
    pid_t initial_parent = getppid();
    if (prctl(PR_SET_PDEATHSIG, SIGKILL) < 0 || getppid() != initial_parent)
        return setup_error("controller parent-death containment");
    struct rlimit no_core = {0, 0};
    if (setrlimit(RLIMIT_CORE, &no_core)) return setup_error("disable owned core dumps");
    int preflight = argc > 1 && !strcmp(argv[1], "preflight");
    int index = preflight ? 2 : 1;
    if (argc <= index) goto usage;
    int pid1 = !strcmp(argv[index], "pid1");
    int pids = !strcmp(argv[index++], "pids");
    unsigned limit = 32;
    uid_t run_uid = getuid();
    gid_t run_gid = getgid();
    int privileged = 0;
    if (argc > index && !strcmp(argv[index], "--as-user")) {
        if (!pid1 || geteuid() != 0 || argc <= index + 2) goto usage;
        char *end_uid, *end_gid;
        unsigned long uid = strtoul(argv[index + 1], &end_uid, 10);
        unsigned long gid = strtoul(argv[index + 2], &end_gid, 10);
        if (*end_uid || *end_gid || !uid || !gid || uid > UINT_MAX || gid > UINT_MAX) goto usage;
        run_uid = (uid_t)uid;
        run_gid = (gid_t)gid;
        privileged = 1;
        index += 3;
    }
    if (geteuid() == 0 && !privileged) {
        errno = EPERM;
        return setup_error("root command execution forbidden; scoped pid1 requires --as-user UID GID");
    }
    if (!pid1 && !pids) goto usage;
    if (!preflight && pids) {
        if (argc <= index + 1 || strcmp(argv[index++], "--limit")) goto usage;
        char *end;
        unsigned long parsed = strtoul(argv[index++], &end, 10);
        if (*end || parsed < 4 || parsed > 1024) goto usage;
        limit = (unsigned)parsed;
    }
    if (!preflight && (argc <= index + 1 || strcmp(argv[index++], "--"))) goto usage;
    if (preflight && argc != index) goto usage;
    struct sigaction original_pipe;
    if (sigaction(SIGPIPE, NULL, &original_pipe) < 0 || signal(SIGPIPE, SIG_IGN) == SIG_ERR)
        return setup_error("controller pipe disposition");
    sigset_t blocked, original_mask;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGCHLD);
    sigaddset(&blocked, SIGTERM);
    sigaddset(&blocked, SIGINT);
    sigaddset(&blocked, SIGHUP);
    if (sigprocmask(SIG_BLOCK, &blocked, &original_mask) < 0 ||
        prctl(PR_SET_CHILD_SUBREAPER, 1) < 0) return setup_error("controller signals");
    int signals = signalfd(-1, &blocked, SFD_CLOEXEC);
    if (signals < 0) return setup_error("controller signalfd");
    if (pids && new_pids_leaf(limit) < 0)
        return setup_error("SEEDBOX_DYNAMORIO_CGROUP_ROOT (empty owned delegated cgroup-v2 pids domain)");
    unsigned long long before = pids ? limit_hits() : 0;
    if (before == ULLONG_MAX) return setup_error("pids.events baseline");
    if (pid1 && (privileged ? unshare(CLONE_NEWPID) : new_pid_namespace()) < 0)
        return setup_error("fresh unprivileged user/PID namespace (enable delegation or supply a scoped launcher)");
    int gate[2];
    if (pipe2(gate, O_CLOEXEC) < 0) return setup_error("launch gate");
    pid_t controller = getpid();
    int controller_handle = (int)syscall(SYS_pidfd_open, controller, 0);
    if (controller_handle < 0) return setup_error("controller pidfd");
    owned = fork();
    if (owned < 0) return setup_error("fork tracee");
    if (!owned) {
        close(gate[1]);
        if (setpgid(0, 0) < 0 || prctl(PR_SET_PDEATHSIG, SIGKILL) < 0) _exit(125);
        /* Inside a PID namespace getppid()==0; the gate also detects an
         * outside controller lost before PR_SET_PDEATHSIG was installed. */
        if (!pid1 && getppid() != controller) _exit(125);
        char ready;
        if (read(gate[0], &ready, 1) != 1) _exit(125);
        close(gate[0]);
        if (privileged && (setgroups(0, NULL) < 0 || setgid(run_gid) < 0 ||
                           setuid(run_uid) < 0 || prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0))
            _exit(125);
        /* Dropping credentials clears PDEATHSIG. Re-arm it, then use the
         * inherited pidfd to close the parent-death race even when getppid()
         * is intentionally zero inside the new PID namespace. */
        if (prctl(PR_SET_PDEATHSIG, SIGKILL) < 0) _exit(125);
        struct pollfd controller_state = { .fd = controller_handle, .events = POLLIN };
        if (poll(&controller_state, 1, 0) != 0) _exit(125);
        close(controller_handle);
        if (sigaction(SIGPIPE, &original_pipe, NULL) < 0) _exit(125);
        sigprocmask(SIG_SETMASK, &original_mask, NULL);
        if (preflight) _exit(!pid1 || getpid() == 1 ? 0 : 125);
        execvp(argv[index], &argv[index]);
        dprintf(STDERR_FILENO, "isolate: exec %s: %s\n", argv[index], strerror(errno));
        _exit(125);
    }
    close(gate[0]);
    close(controller_handle);
    if (setpgid(owned, owned) < 0 && errno != EACCES) return setup_error("owned process group");
    if (pids) {
        char value[80];
        snprintf(value, sizeof(value), "%ld", (long)owned);
        if (leaf_write("cgroup.procs", value) < 0) return setup_error("move owned tracee to pids leaf");
    }
    if (write(gate[1], "1", 1) != 1) return setup_error("release tracee");
    close(gate[1]);
    int status;
    if (wait_owned(signals) < 0) return setup_error("tracee positive completion");
    if (pids) {
        unsigned long long after = limit_hits();
        if (after == ULLONG_MAX || after < before) return setup_error("pids.events result");
        fprintf(stderr, "isolate: pids-limit-hits=%llu\n", after - before);
        if (!preflight && after == before) { errno = EPROTO; return setup_error("no owned pids limit hit"); }
    }
    /* WNOWAIT retained the direct child's PID until all fallible evidence
     * reads finished. Terminate its group before releasing that PID. */
    kill(-owned, SIGKILL);
    if (waitpid(owned, &status, 0) != owned) return setup_error("reap tracee");
    owned = -1;
    if (cleanup() < 0) return 2;
    close(signals);
    if (WIFSIGNALED(status)) {
        int sig = WTERMSIG(status);
        signal(sig, SIG_DFL);
        sigset_t fatal;
        sigemptyset(&fatal);
        sigaddset(&fatal, sig);
        sigprocmask(SIG_UNBLOCK, &fatal, NULL);
        raise(sig);
        return 128 + sig;
    }
    if (!WIFEXITED(status)) return 2;
    if (preflight && WEXITSTATUS(status) == 0)
        fprintf(stderr, "isolate: preflight %s\n", pid1 ? "pid1" : "pids");
    return WEXITSTATUS(status);
usage:
    fprintf(stderr, "isolate: usage: pid1 -- COMMAND... | pids --limit N -- COMMAND... | preflight pid1|pids\n");
    return 2;
}
