#include <stdatomic.h>
#include <stdio.h>

#ifdef __aarch64__
#include <sys/wait.h>
#include <unistd.h>

static int check_fixture(const char *path)
{
    pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 1;
    }
    if (child == 0) {
        execl(path, path, (char *)NULL);
        perror(path);
        _exit(127);
    }
    int status;
    if (waitpid(child, &status, 0) != child) {
        perror("waitpid");
        return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "%s: wait status %d\n", path, status);
        return 1;
    }
    return 0;
}
#endif

int main(void)
{
    atomic_uint counter = ATOMIC_VAR_INIT(0);
    for (unsigned i = 0; i < 100000; ++i) {
        unsigned expected = atomic_load_explicit(&counter, memory_order_relaxed);
        while (!atomic_compare_exchange_weak_explicit(
            &counter, &expected, expected + 1, memory_order_seq_cst,
            memory_order_relaxed)) {
        }
    }
    unsigned result = atomic_load(&counter);
    if (result != 100000) {
        fprintf(stderr, "atomic counter: %u\n", result);
        return 1;
    }
#ifdef __aarch64__
    const char *fixtures[] = {
        "/exclusive-epoch", "/exclusive-overlap", "/exclusive-consume",
        "/exclusive-discard", "/exclusive-fault", "/exclusive-fault-pair",
        "/exclusive-fault-stolen",
    };
    for (unsigned i = 0; i < sizeof(fixtures) / sizeof(fixtures[0]); ++i) {
        if (check_fixture(fixtures[i]) != 0)
            return 1;
    }
#endif
    printf("atomic counter: %u\n", result);
    puts("exclusive checks: passed");
    return 0;
}
