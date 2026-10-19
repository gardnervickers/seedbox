/* Public ISA-feature control adapted from Enrico Deiana's distant B.cond test
 * in DynamoRIO 1823dc5bfcde301c04704d1154a2eb0451345c1b,
 * suite/tests/api/drdecode_aarch64.c:test_isa_features (see DynamoRIO-LICENSE.txt).
 * Adds near/far ADR/ADRP through public operands; no encoded target is executed.
 * Reachability must not restrict instruction feature identification.
 */
#include "dr_api.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void)
{
    byte *pc = (byte *)(uintptr_t)0x100000000ULL;
    for (int far = 0; far < 2; far++) {
        for (int op = 0; op < 3; op++) {
            uintptr_t delta = far ? (op == 2 ? 0x1000000000ULL : 0x1000000ULL)
                                  : 0x1000ULL;
            uintptr_t address = (uintptr_t)pc + delta;
            opnd_t target = op == 0 ? opnd_create_pc((app_pc)address)
                                    : opnd_create_rel_addr((void *)address, OPSZ_0);
            instr_t *in;
            if (op == 0) {
                in = INSTR_CREATE_bcond(GLOBAL_DCONTEXT, target);
                instr_set_predicate(in, DR_PRED_EQ);
            } else if (op == 1) {
                in = INSTR_CREATE_adr(GLOBAL_DCONTEXT, opnd_create_reg(DR_REG_X0), target);
            } else {
                in = INSTR_CREATE_adrp(GLOBAL_DCONTEXT, opnd_create_reg(DR_REG_X0), target);
            }
            uint feature = instr_get_isa_feature(pc, in);
            const char *name = instr_get_isa_feature_name(feature);
            if (feature != ISA_FEAT_BASE || strcmp(name, "BASE")) {
                fprintf(stderr, "query op=%d far=%d feature=%u name=%s\n",
                        op, far, feature, name);
                abort();
            }
            printf("%s %s BASE\n", op == 0 ? "B.cond" : op == 1 ? "ADR" : "ADRP",
                   far ? "far" : "near");
            instr_destroy(GLOBAL_DCONTEXT, in);
        }
    }
    return 0;
}
