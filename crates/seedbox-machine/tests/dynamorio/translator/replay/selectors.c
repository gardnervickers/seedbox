/* Public source-selector controls for the original 356c1821 A64 population.
 * These exercise enum/IR APIs only; no A64 instruction is executed. */
#include "dr_api.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define REPLAY_CHECK(c, ...) do { \
    if (!(c)) { fprintf(stderr, __VA_ARGS__); fputc('\n', stderr); abort(); } \
} while (0)
#include "source_selectors.h"

int
main(void)
{
#ifdef DYNAMORIO_STANDALONE
    REPLAY_CHECK(dr_standalone_init() != NULL, "standalone initialization");
    atexit(dr_standalone_exit);
#endif
    REPLAY_CHECK(SOURCE_REG_COUNT == 410, "original selector population changed");
    for (unsigned i = 0; i < SOURCE_REG_COUNT; ++i) {
        reg_id_t reg = source_reg(i);
        REPLAY_CHECK(reg == i, "source register %u changed numeric identity to %u", i, reg);
        REPLAY_CHECK(source_reg_id(reg) == i, "reverse selector %u", i);
        REPLAY_CHECK(source_reg(source_reg_id(reg)) == reg, "roundtrip register %u", reg);
    }
    static const reg_id_t restored[] = {
        DR_REG_CONTEXTIDR_EL1, DR_REG_ELR_EL1, DR_REG_SPSR_EL1,
        DR_REG_TPIDR_EL1, DR_REG_ACCDATA_EL1,
    };
    static const char *const names[] = {
        "contextidr_el1", "elr_el1", "spsr_el1", "tpidr_el1", "accdata_el1",
    };
    for (unsigned i = 0; i < 5; ++i) {
        REPLAY_CHECK(restored[i] == 405 + i, "restored register numeric ID %u", i);
        REPLAY_CHECK(strcmp(get_register_name(restored[i]), names[i]) == 0,
                     "restored register name %u", i);
        REPLAY_CHECK(source_reg(405 + i) == restored[i] &&
                     source_reg_id(restored[i]) == 405 + i,
                     "restored register selector %s", names[i]);
    }
    unsigned aliases = 0;
    for (unsigned i = 0; i < 31; ++i) {
        REPLAY_CHECK(source_reg_id(DR_REG_R0 + i) == source_reg_id(DR_REG_X0 + i),
                     "R/X alias %u", i);
        ++aliases;
    }
    static const reg_id_t pairs[][2] = {
        { DR_REG_SP, DR_REG_XSP }, { DR_REG_LR, DR_REG_X30 },
        { DR_REG_MAX_ADDRESSING_REG, DR_REG_Z31 },
        { DR_REG_TPIDR_EL0, DR_REG_TPIDRURW },
        { DR_REG_TPIDRRO_EL0, DR_REG_TPIDRURO },
    };
    for (unsigned i = 0; i < sizeof(pairs) / sizeof(pairs[0]); ++i) {
        REPLAY_CHECK(pairs[i][0] == pairs[i][1] &&
                     source_reg_id(pairs[i][0]) == source_reg_id(pairs[i][1]) &&
                     source_reg(source_reg_id(pairs[i][0])) == pairs[i][1],
                     "alias pair %u", i);
        ++aliases;
    }
    for (unsigned i = 1223; i <= 1226; ++i)
        REPLAY_CHECK(source_opcode(i - 6) == (int)i, "LS64 opcode selector %u", i);
    printf("source-selectors: PASS registers=410 restored=5 aliases=%u opcodes=4\n", aliases);
    return 0;
}
