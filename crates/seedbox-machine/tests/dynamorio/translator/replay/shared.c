/* Seedbox public API controls. Derived property names refer to DR 356c1821
 * suite/proptest/a64_ir_transform.c; these small cross-ISA controls supplement,
 * and do not replace, that file's complete A64 populations. */
#include "dr_api.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef DYNAMORIO_STANDALONE
static void *replay_context;
# define GD replay_context
#else
# define GD GLOBAL_DCONTEXT
#endif
#ifdef AARCH64
# define FIRST_REG DR_REG_X0
# define SECOND_REG DR_REG_X1
#else
# define FIRST_REG DR_REG_XAX
# define SECOND_REG DR_REG_XCX
#endif
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "%s:%d: %s\n", __FILE__, __LINE__, #c); exit(1); } } while (0)

int main(void)
{
#ifdef DYNAMORIO_STANDALONE
    replay_context = dr_standalone_init();
    if (replay_context == NULL)
        return 2;
    atexit(dr_standalone_exit);
#endif
    /* Fresh IR and encoding storage for every iteration, no executable memory. */
    for (int i = 0; i < 256; ++i) {
        byte encoded[64], again[64], moved[64];
        instr_t *made = XINST_CREATE_load_int(GD, opnd_create_reg(FIRST_REG),
                                             OPND_CREATE_INTPTR(i));
        byte *end = instr_encode(GD, made, encoded);
        CHECK(end != NULL && end > encoded && end <= encoded + sizeof(encoded));
        instr_t *decoded = instr_create(GD);
        CHECK(decode(GD, encoded, decoded) == end);
        ptr_int_t value = -1;
        CHECK(instr_is_mov_constant(decoded, &value) && value == i);
        instr_t *clone = instr_clone(GD, decoded);
        CHECK(instr_same(clone, decoded));
        instr_destroy(GD, decoded);
        instr_set_translation(clone, (byte *)(ptr_uint_t)0x400000);
        instr_set_raw_bits_valid(clone, false);
        byte *again_end = instr_encode(GD, clone, again);
        CHECK(again_end != NULL && again_end - again == end - encoded);
        CHECK(memcmp(encoded, again, end - encoded) == 0);
        byte *moved_end = instr_encode_to_copy(GD, clone, moved,
                                              (byte *)(ptr_uint_t)0x800000);
        CHECK(moved_end != NULL && moved_end - moved == end - encoded);
        CHECK(memcmp(encoded, moved, end - encoded) == 0);
        instr_t *different = XINST_CREATE_load_int(GD, opnd_create_reg(FIRST_REG),
                                                  OPND_CREATE_INTPTR(i + 1));
        CHECK(!instr_same(made, different));
        instr_destroy(GD, different);
        instr_t *renamed = instr_clone(GD, clone);
        instr_replace_reg_resize(renamed, FIRST_REG, SECOND_REG);
        CHECK(instr_writes_to_reg(renamed, SECOND_REG, DR_QUERY_DEFAULT));
        CHECK(!instr_writes_to_reg(renamed, FIRST_REG, DR_QUERY_DEFAULT));
        instr_replace_reg_resize(renamed, SECOND_REG, FIRST_REG);
        CHECK(instr_same(renamed, clone));
        instr_t *deps = instr_create(GD);
        instr_convert_to_isa_regdeps(GD, clone, deps);
        CHECK(instr_num_dsts(deps) == 1 && instr_num_srcs(deps) == 0);
        instr_destroy(GD, deps);
        instrlist_t *list = instrlist_create(GD);
        instr_t *label = INSTR_CREATE_label(GD);
        instr_t *jump = XINST_CREATE_jump(GD, opnd_create_instr(label));
        instrlist_append(list, jump);
        instrlist_append(list, renamed);
        instrlist_append(list, label);
        byte list_bytes[128];
        byte *list_end = instrlist_encode(GD, list, list_bytes, true);
        CHECK(list_end != NULL);
        instr_t *branch = instr_create(GD);
        CHECK(decode(GD, list_bytes, branch) != NULL);
        CHECK(opnd_get_pc(instr_get_target(branch)) == list_end);
        instr_destroy(GD, branch);
        instrlist_clear_and_destroy(GD, list);
        instr_destroy(GD, clone);
        instr_destroy(GD, made);
    }
    puts("shared-public-ir: PASS cases=256 properties=8 negative-controls=256");
    return 0;
}
