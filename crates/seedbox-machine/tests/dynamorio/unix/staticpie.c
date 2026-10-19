/* Source Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/probes/c/cfzpie.c. */
/*
 * Static-PIE helper for cfz's staticpie test, built with -static-pie:
 *   cfzpie start   sets ADDR_NO_RANDOMIZE and re-executes /proc/self/exe twice.
 * Each pass prints one line describing its self-relocation and personality.
 */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/personality.h>
#include <unistd.h>

static int value = 42;
static int *pointer = &value; /* R_AARCH64_RELATIVE, applied by the program itself */
static int seven(void) { return 7; }
static int (*function)(void) = seven;
extern char **environ;
extern const Elf64_Ehdr __ehdr_start; /* the linker's name for this program's own ELF header */

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "start")) {
        int old = personality(0xffffffffUL);
        if (old < 0 || personality(old | ADDR_NO_RANDOMIZE) < 0) { printf("UNSUPPORTED personality %s\n", strerror(errno)); return 0; }
        char *args[] = {argv[0], "again", "1", NULL};
        execve("/proc/self/exe", args, environ);
        printf("execError %s\n", strerror(errno));
        return 2;
    }
    if (argc > 2 && !strcmp(argv[1], "again")) {
        int pass = atoi(argv[2]);
        const Elf64_Phdr *ph = (const Elf64_Phdr *)getauxval(AT_PHDR);
        unsigned long n = getauxval(AT_PHNUM);
        int interp = 0, type = __ehdr_start.e_type;
        for (unsigned long i = 0; i < n; i++)
            if (ph[i].p_type == PT_INTERP) interp = 1;
        int persona = personality(0xffffffffUL);
        printf("pass %d relocated %d value %d func %d noRandomize %d interp %d atBase %d etDyn %d phdrInImage %d addr %p\n", pass,
               pointer == &value, *pointer, function(), persona >= 0 && (persona & ADDR_NO_RANDOMIZE) != 0, interp,
               getauxval(AT_BASE) != 0, type == ET_DYN,
               (uintptr_t)ph == (uintptr_t)&__ehdr_start + __ehdr_start.e_phoff, (void *)pointer);
        if (pass < 2) {
            char *args[] = {argv[0], "again", "2", NULL};
            execve("/proc/self/exe", args, environ);
            printf("execError %s\n", strerror(errno));
            return 2;
        }
        return 0;
    }
    return 2;
}
