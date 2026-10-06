#ifndef SEEDBOX_WITNESS_DECODE_H
#define SEEDBOX_WITNESS_DECODE_H
#include <stddef.h>

/* This pinned guest uses these legacy/REX prefixes. Instruction length comes
 * from QEMU, so never interpret an immediate or displacement as an opcode. */
static size_t opcode_offset(const unsigned char *bytes, size_t size) {
    size_t p = 0;
    while (p < size) {
        unsigned b = bytes[p];
        if ((b >= 0x40 && b <= 0x4f) || b == 0x26 || b == 0x2e || b == 0x36 || b == 0x3e ||
            b == 0x64 || b == 0x65 || b == 0x66 || b == 0x67 || b == 0xf0 || b == 0xf2 || b == 0xf3) ++p;
        else break;
    }
    return p;
}

/* 1: OUT through DX (check its value at dispatch); 2: OUT immediate 0xf4. */
static int terminal_out(const unsigned char *bytes, size_t size) {
    size_t p = opcode_offset(bytes, size);
    if (p == size) return 0;
    if (size == p + 1 && (bytes[p] == 0xee || bytes[p] == 0xef)) return 1;
    if (size == p + 2 && (bytes[p] == 0xe6 || bytes[p] == 0xe7) && bytes[p + 1] == 0xf4) return 2;
    return 0;
}
#endif
