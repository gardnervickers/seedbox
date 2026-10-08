#include "environment.h"
#include <assert.h>

int main(void) {
    /* Empty environment and unrelated entries both get exactly one default.
     * Prefix lookalikes are unrelated, and their order/bytes stay intact. */
    for (long cpus = 1; cpus <= 4; cpus += 3) {
        for (size_t count = 0; count <= 3; count += 3) {
            char *original[] = {"PATH=/bin:/usr/bin", "GOMAXPROCS_EXTRA=7", "OTHER=a=b\nc"};
            char **environment = calloc(count + 1, sizeof(char *)); assert(environment);
            for (size_t i = 0; i < count; i++) environment[i] = original[i];
            assert(!seedbox_default_gomaxprocs(&environment, cpus));
            for (size_t i = 0; i < count; i++) assert(environment[i] == original[i]);
            assert(!strcmp(environment[count], cpus == 1 ? "GOMAXPROCS=1" : "GOMAXPROCS=4"));
            assert(!environment[count + 1]);
            char **saved = environment;
            assert(!seedbox_default_gomaxprocs(&environment, 8));
            assert(environment == saved && !environment[count + 1]);
            free(environment[count]); free(environment);
        }
    }
    /* An image/Compose override is authoritative, even empty or nonnumeric. */
    char *overrides[] = {"GOMAXPROCS=4", "GOMAXPROCS=", "GOMAXPROCS=invalid"};
    for (size_t i = 0; i < sizeof(overrides)/sizeof(*overrides); i++) {
        char *entries[] = {"PATH=/bin", overrides[i], "OTHER=unchanged", NULL};
        char **environment = entries;
        assert(!seedbox_default_gomaxprocs(&environment, 1));
        assert(environment == entries && environment[1] == overrides[i] && !environment[3]);
    }
    for (long cpus = -1; cpus <= 0; cpus++) {
        char **environment = calloc(1, sizeof(char *)); assert(environment);
        char **saved = environment;
        errno = 0;
        assert(seedbox_default_gomaxprocs(&environment, cpus) == -1 && errno == EINVAL);
        assert(environment == saved && !environment[0]);
        free(environment);
    }
}
