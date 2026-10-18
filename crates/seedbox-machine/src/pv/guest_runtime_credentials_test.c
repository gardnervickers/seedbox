/* Exercise libc's named-ID results as well as numeric recipe IDs. Sentinel
 * identities must fail before any group/credential syscall, even when DR
 * startup temporarily retains saved root. This test needs no host privilege. */
#define _GNU_SOURCE
#include <pwd.h>
#include <grp.h>
#include <stdint.h>
#include <string.h>
static struct passwd *sentinel_passwd(const char *name) {
    static struct passwd value;
    if (strcmp(name, "uid-sentinel")) return NULL;
    value.pw_name = "uid-sentinel";
    value.pw_uid = (uid_t)-1;
    value.pw_gid = 2345;
    return &value;
}
static struct group *sentinel_group(const char *name) {
    static struct group value;
    if (strcmp(name, "gid-sentinel")) return NULL;
    value.gr_name = "gid-sentinel";
    value.gr_gid = (gid_t)-1;
    return &value;
}
#define getpwnam sentinel_passwd
#define getgrnam sentinel_group
#include "guest_application.c"
#undef getpwnam
#undef getgrnam
int main(void) {
    const char *invalid[] = { "4294967295:2345", "1234:4294967295",
                              "uid-sentinel", "1234:gid-sentinel" };
    uid_t before = getuid(), effective = geteuid();
    gid_t group = getgid();
    for (unsigned i = 0; i < sizeof(invalid) / sizeof(invalid[0]); ++i) {
        uid_t final_uid;
        errno = 0;
        if (credentials(invalid[i], 1, &final_uid) != -1 || errno != EINVAL ||
            getuid() != before || geteuid() != effective || getgid() != group)
            return 1;
    }
    puts("runtime credential sentinels rejected before mutation");
    return 0;
}
