/* Only public application instructions. No DR layouts or internal calls. */
.text
.macro publisher name, operand
.global \name
.type \name, %function
\name:
    stp x28, x30, [sp, #-16]!
    mov x9, x0
    mov x10, x1
    mov w11, #1
.Lpublish\@:
    /* One aligned 4-byte instruction; retain the existing RET. */
    mov w12, #0x20
    movk w12, #0x5280, lsl #16
    cmp w11, #1
    b.eq .Lstore\@
    add w12, w12, #0x20
.Lstore\@:
    str w12, [x9]
    dc cvau, x9
    dsb ish
    mov \operand, x9
    ic ivau, \operand
    dsb ish
    stlr w11, [x10]
    add x13, x10, #4
    add x14, x10, #8
.Lwait\@:
    ldar w12, [x13]
    cmp w12, w11
    b.eq .Lcomplete\@
    stlr w11, [x14]
    b .Lwait\@
.Lcomplete\@:
    add w11, w11, #1
    cmp w11, #3
    b.ne .Lpublish\@
    ldp x28, x30, [sp], #16
    ret
.size \name, .-\name
.endm
publisher publish_x0, x0
publisher publish_x28, x28
publisher publish_x30, x30
.section .note.GNU-stack,"",%progbits
