#define _GNU_SOURCE
#include "guest_application.h"
#include "guest_launch_path.h"
#include <grp.h>
#include <pwd.h>
#include <stdint.h>
#include <limits.h>
extern char **environ;

static int exact(int fd, void *buffer, size_t length) {
    unsigned char *bytes = buffer;
    while (length) {
        ssize_t count = read(fd, bytes, length);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) { if (!count) errno = EINVAL; return -1; }
        bytes += count; length -= (size_t)count;
    }
    return 0;
}
static int count(int fd, uint32_t *out) {
    unsigned char bytes[4];
    if (exact(fd, bytes, sizeof(bytes))) return -1;
    *out = (uint32_t)bytes[0] | (uint32_t)bytes[1] << 8 |
           (uint32_t)bytes[2] << 16 | (uint32_t)bytes[3] << 24;
    return 0;
}
static int text(int fd, char **out) {
    uint32_t length;
    if (count(fd, &length)) return -1;
    if (length > 16 * 1024 * 1024) { errno = E2BIG; return -1; }
    char *value = malloc((size_t)length + 1);
    if (!value) return -1;
    if (exact(fd, value, length)) { int error = errno; free(value); errno = error; return -1; }
    if (memchr(value, 0, length)) { free(value); errno = EINVAL; return -1; }
    value[length] = 0; *out = value; return 0;
}
static int list(int fd, char ***out) {
    uint32_t length;
    if (count(fd, &length)) return -1;
    if (length > 65536) { errno = E2BIG; return -1; }
    char **values = calloc((size_t)length + 1, sizeof(*values));
    if (!values) return -1;
    *out = values;
    for (uint32_t i = 0; i < length; ++i) if (text(fd, &values[i])) return -1;
    return 0;
}
static void free_list(char **values) {
    if (!values) return;
    for (char **item = values; *item; ++item) free(*item);
    free(values);
}
void sbx_application_destroy(struct sbx_application *application) {
    free_list(application->arguments); free_list(application->execution_prefix); free_list(application->environment);
    free(application->working_directory); free(application->user);
    memset(application, 0, sizeof(*application));
}
int sbx_application_load(struct sbx_application *application, const char *path) {
    memset(application, 0, sizeof(*application));
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    unsigned char magic[8];
    int result = exact(fd, magic, sizeof(magic));
    if (!result && memcmp(magic, "SBXAPP2\0", sizeof(magic))) { errno = EINVAL; result = -1; }
    if (!result) result = list(fd, &application->arguments);
    if (!result) result = list(fd, &application->execution_prefix);
    if (!result) result = list(fd, &application->environment);
    if (!result) result = text(fd, &application->working_directory);
    if (!result) result = text(fd, &application->user);
    if (!result && (!application->arguments[0] || !application->arguments[0][0] ||
                    application->working_directory[0] != '/')) { errno = EINVAL; result = -1; }
    if (!result) for (char **item = application->environment; *item; ++item) {
        char *equals = strchr(*item, '=');
        if (!equals || equals == *item) { errno = EINVAL; result = -1; break; }
    }
    if (!result) {
        unsigned char tail; ssize_t size;
        do { size = read(fd, &tail, 1); } while (size < 0 && errno == EINTR);
        if (size != 0) { if (size > 0) errno = EINVAL; result = -1; }
    }
    int error = errno; close(fd);
    if (result) sbx_application_destroy(application);
    errno = error; return result;
}
static int saved_credentials(uid_t real, uid_t effective, uid_t saved) {
#ifdef __linux__
    return setresuid(real, effective, saved);
#else
    if (real == effective && effective == saved) return setuid(real);
    errno = ENOTSUP; return -1;
#endif
}
static int credentials(const char *user, int prepare_runtime, uid_t *final_uid) {
    *final_uid = getuid();
    if (!*user) return 0;
    char *copy = strdup(user);
    if (!copy) return -1;
    char *group = strchr(copy, ':'); if (group) *group++ = 0;
    struct passwd *pw = getpwnam(copy);
    char *end = NULL; errno = 0; unsigned long number = strtoul(copy, &end, 10);
    uid_t uid; gid_t gid;
    if (pw) { uid = pw->pw_uid; gid = pw->pw_gid; }
    else if (*copy && end && !*end && !errno && number < UINT32_MAX) {
        uid = (uid_t)number; pw = getpwuid(uid); gid = pw ? pw->pw_gid : 0;
    } else { free(copy); errno = EINVAL; return -1; }
    if (group) {
        struct group *gr = getgrnam(group); errno = 0; number = strtoul(group, &end, 10);
        if (gr) gid = gr->gr_gid;
        else if (*group && !*end && !errno && number < UINT32_MAX) gid = (gid_t)number;
        else { free(copy); errno = EINVAL; return -1; }
    }
    if (uid == (uid_t)-1 || gid == (gid_t)-1) { free(copy); errno = EINVAL; return -1; }
    int result = pw && !group ? initgroups(pw->pw_name, gid) : setgroups(0, NULL);
    free(copy);
    *final_uid = uid;
    if (result || setgid(gid)) return -1;
    /* Only the private DR startup path retains saved root while selecting and
     * opening its executable. PATH and execute checks use the target euid. */
    if (prepare_runtime ? saved_credentials(0, uid, 0) : setuid(uid)) return -1;
    return 0;
}
/* The descriptor grants the runtime startup loader exactly the read access
 * needed to instrument the already-authorized target, without changing the
 * workload inode, mode, argv or credentials. Never expose it to app code. */
static int runtime_open(const char *path) {
    uid_t target = geteuid();
    if (seteuid(0)) return -1;
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    int error = errno;
    if (seteuid(target)) { if (fd >= 0) close(fd); return -1; }
    errno = error;
    return fd;
}
int sbx_application_exec(struct sbx_application *application) {
    int runtime = application->execution_prefix[0] &&
        !strcmp(application->execution_prefix[0], "/seedbox/progress/bin64/drrun");
    int privileged_runtime = runtime && geteuid() == 0;
    uid_t final_uid;
    if (chdir(application->working_directory) ||
        credentials(application->user, privileged_runtime, &final_uid)) return errno;
    environ = application->environment;
    /* The guest library tells an unreachable socket from running outside Seedbox. */
    if (application->guest_socket && setenv("SEEDBOX_GUEST_SOCKET", application->guest_socket, 1))
        return errno;
    if (runtime) unsetenv("SEEDBOX_DR_EXECUTABLE_FD");
    if (!application->execution_prefix[0]) {
        /* Native exec semantics permit execute-only ELF files and handle PATH
         * candidates before any optional userspace header inspection. musl's
         * execvp does not supply the legacy no-shebang shell fallback. */
        execvp(application->arguments[0], application->arguments);
        if (errno != ENOEXEC) return errno;
    }
    int executable_fd = -1;
    char **arguments = privileged_runtime ?
        sbx_script_arguments_open(application->arguments, runtime_open, &executable_fd) :
        sbx_script_arguments(application->arguments);
    int preparation_error = errno;
    /* No privilege may survive into drrun, a wrapper, or a failure caller. */
    if (privileged_runtime && saved_credentials(final_uid, final_uid, final_uid)) {
        if (executable_fd >= 0) close(executable_fd);
        return errno;
    }
    if (!arguments) { errno = preparation_error; return errno; }
    if (executable_fd >= 0) {
        char *path = sbx_executable(arguments[0], 1);
        struct stat selected, opened;
        int authorized = path && !access(path, X_OK) && !stat(path, &selected) &&
            !fstat(executable_fd, &opened) && S_ISREG(opened.st_mode) &&
            selected.st_dev == opened.st_dev && selected.st_ino == opened.st_ino;
        free(path);
        if (!authorized) { close(executable_fd); return EACCES; }
        char value[32];
        snprintf(value, sizeof(value), "%d", executable_fd);
        if (fcntl(executable_fd, F_SETFD, 0) ||
            setenv("SEEDBOX_DR_EXECUTABLE_FD", value, 1)) {
            int error = errno; close(executable_fd); return error;
        }
    }
    /* Resolve the original script/interpreter first. An execution runtime's
     * ELF loader must not hide that original target from shebang resolution. */
    size_t prefix = 0, count = 0;
    while (application->execution_prefix[prefix]) ++prefix;
    while (arguments[count]) ++count;
    if (prefix) {
        char **wrapped = calloc(prefix + count + 1, sizeof(*wrapped));
        if (!wrapped) {
            int error = errno;
            if (executable_fd >= 0) close(executable_fd);
            if (runtime) unsetenv("SEEDBOX_DR_EXECUTABLE_FD");
            return error;
        }
        for (size_t i = 0; i < prefix; ++i) wrapped[i] = application->execution_prefix[i];
        for (size_t i = 0; i < count; ++i) wrapped[prefix + i] = arguments[i];
        arguments = wrapped;
    }
    execvp(arguments[0], arguments);
    int error = errno;
    if (executable_fd >= 0) close(executable_fd);
    if (runtime) unsetenv("SEEDBOX_DR_EXECUTABLE_FD");
    return error;
}
