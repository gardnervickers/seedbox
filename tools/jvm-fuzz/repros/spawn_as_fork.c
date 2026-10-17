/*
 * LD_PRELOAD workaround experiment: run vfork() and simple posix_spawn() calls as fork()+execve().
 *
 * A vfork parent waits in a killable kernel state, so DynamoRIO's suspend signal cannot reach it.
 * A synch-all started by another thread (for example after a code write) then waits forever, and
 * the vfork child, still inside DynamoRIO before execve, waits behind it. With fork the parent
 * returns at once and stays suspendable.
 *
 * Only posix_spawn/posix_spawnp calls without file actions or attributes are rewritten (the JDK's
 * jspawnhelper launch); others go to libc. Exec failures surface as exit 127 instead of an error.
 * Build: gcc -O2 -shared -fPIC -o spawn_as_fork.so spawn_as_fork.c -ldl
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <spawn.h>
#include <unistd.h>

typedef int (*spawn_fn)(pid_t *, const char *, const posix_spawn_file_actions_t *, const posix_spawnattr_t *,
                        char *const[], char *const[]);

static int spawn_with_fork(pid_t *pid, const char *path, char *const argv[], char *const envp[], int search) {
    pid_t child = fork();
    if (child < 0) return errno;
    if (child == 0) {
        if (search) execvpe(path, argv, envp);
        else execve(path, argv, envp);
        _exit(127);
    }
    if (pid) *pid = child;
    return 0;
}

int posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *actions,
                const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
    if (!actions && !attr) return spawn_with_fork(pid, path, argv, envp, 0);
    spawn_fn real = (spawn_fn)dlsym(RTLD_NEXT, "posix_spawn");
    return real(pid, path, actions, attr, argv, envp);
}

int posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *actions,
                 const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
    if (!actions && !attr) return spawn_with_fork(pid, file, argv, envp, 1);
    spawn_fn real = (spawn_fn)dlsym(RTLD_NEXT, "posix_spawnp");
    return real(pid, file, actions, attr, argv, envp);
}

pid_t vfork(void) { return fork(); }
