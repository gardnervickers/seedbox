/* Source oracle: DynamoRIO 356c1821045a4b5678d0813e132074324623df6e,
 * suite/tests/api/dis-a64.c (license in licenses/dis-a64.c.txt).
 * Each inventory row gets a fresh instruction and its exact source ID. */
#include "dr_api.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv)
{
    if (argc != 2) return 2;
    FILE *f = fopen(argv[1], "r");
    if (!f) return 2;
    /* drdecode needs no runtime initialization.  Match the source offline
     * test's enable_all_test_cpu_features() vector-length setting as well. */
    void *dc = GLOBAL_DCONTEXT;
    if (!dr_set_vector_length(256)) return 2;
    char *line = NULL;
    size_t cap = 0, count = 0;
    int failures = 0;
    while (getline(&line, &cap, f) > 0) {
        char *tab = strchr(line, '\t');
        if (!tab) return 2;
        *tab++ = 0;
        char *end;
        uint32_t word = strtoul(tab, &end, 16), alternate = 0;
        char *assembly = strchr(end, ':');
        char *expected = assembly ? strchr(assembly + 1, ':') : NULL;
        if (!expected) return 2;
        expected++;
        while (*expected == ' ') expected++;
        char *extra = strchr(expected, ':');
        if (extra) {
            alternate = strtoul(expected, &end, 16);
            expected = extra + 1;
            while (*expected == ' ') expected++;
        }
        expected[strcspn(expected, "\r\n")] = 0;
        char text[1024] = {0};
        byte *pc = (byte *)(uintptr_t)0x10000000;
        instr_t ins;
        instr_init(dc, &ins);
        uint32_t encoded = 0;
        byte *decoded = decode_from_copy(dc, (byte *)&word, pc, &ins);
        disassemble_to_buffer(dc, (byte *)&word, pc, false, false, text, sizeof(text), NULL);
        text[strcspn(text, "\r\n")] = 0;
        byte *encoded_end = decoded ? instr_encode_to_copy(dc, &ins, (byte *)&encoded, pc) : NULL;
        int ok = decoded == (byte *)&word + 4 && strcmp(text, expected) == 0 &&
            encoded_end == (byte *)&encoded + 4 && (encoded == word || encoded == alternate);
        printf("%s: %s\n", line, ok ? "PASS" : "FAIL");
        if (!ok) {
            fprintf(stderr, "%s: word=%08x encoded=%08x alternate=%08x\nexpected <%s>\nactual <%s>\n",
                    line, word, encoded, alternate, expected, text);
            failures++;
        }
        instr_free(dc, &ins);
        count++;
    }
    free(line);
    fclose(f);
    if (count != 5079) { fprintf(stderr, "expected 5079 source rows, got %zu\n", count); return 1; }
    return failures ? 1 : 0;
}
