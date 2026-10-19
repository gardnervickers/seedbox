/* Derived public control for the FD-limit port defect detected in R6;
 * not completion of an original PR317 source case. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <unistd.h>

int
main(void)
{
    /* The Rust owner supplies a fresh working directory and removes it even
     * if the supervised application fails or must be terminated. */
    const char *name = "retained";
    int retained = open(name, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (retained < 0) {
        perror("create retained file");
        return 2;
    }
    int result = 2, opened = -1;
    struct rlimit limit;
    if (write(retained, "keep-data", 9) != 9 || getrlimit(RLIMIT_NOFILE, &limit)) {
        perror("prepare retained file");
        goto cleanup;
    }
    /* Only this dedicated application's soft limit changes. The retained
     * descriptor remains legal even though it is above the new limit. */
    limit.rlim_cur = 3;
    if (setrlimit(RLIMIT_NOFILE, &limit)) {
        perror("setrlimit");
        goto cleanup;
    }
    errno = 0;
    opened = open(name, O_WRONLY | O_TRUNC);
    const int error = errno;
    struct stat status;
    char contents[10] = { 0 };
    if (fstat(retained, &status)) {
        perror("fstat retained fd");
        goto cleanup;
    }
    const ssize_t bytes = pread(retained, contents, sizeof(contents), 0);
    const int unchanged = status.st_size == 9 && bytes == 9 &&
                          memcmp(contents, "keep-data", 9) == 0;
    printf("fd-limits truncate: open=%d errno=%d retained_bytes=%ld contents-match=%d\n",
           opened, error, (long)status.st_size, unchanged);
    result = opened == -1 && error == EMFILE && unchanged ? 0 : 1;
cleanup:
    if (opened >= 0)
        close(opened);
    /* Rust also inspects the bytes after exit, including a fatal runtime error. */
    if (close(retained)) {
        perror("cleanup retained file");
        return 2;
    }
    return result;
}
