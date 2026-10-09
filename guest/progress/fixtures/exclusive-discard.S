// STXR/STXP with WZR status still require a deterministic commit, and must
// preserve every application register borrowed for the hidden retry status.
.global _start
.text
_start:
    adrp x3, value
    add x3, x3, :lo12:value
    mov x1, #11
    str x1, [x3]
    mov w12, #1000
single:
    ldxr x0, [x3]
    mov x1, #31
    stxr wzr, x1, [x3]
    cmp x0, #11
    b.ne bad_single_register
    ldr x2, [x3]
    cmp x2, #31
    b.eq consumed
    subs w12, w12, #1
    b.ne single
    mov w0, #2
    b exit
consumed:
    mov x1, #99
    stxr wzr, x1, [x3]
    ldr x2, [x3]
    cmp x2, #31
    b.ne bad_consumption
    mov x0, #11
    mov x1, #12
    stp x0, x1, [x3]
    mov x2, #42
    mov x4, #44
    mov x5, #45
    mov x6, #46
    mov x7, #47
    mov w12, #1000
pair:
    ldaxp x0, x1, [x3]
    mov x0, #31
    mov x1, #32
    stlxp wzr, x0, x1, [x3]
    cmp x2, #42
    b.ne bad_pair_register
    cmp x4, #44
    b.ne bad_pair_register
    cmp x5, #45
    b.ne bad_pair_register
    cmp x6, #46
    b.ne bad_pair_register
    cmp x7, #47
    b.ne bad_pair_register
    ldp x8, x9, [x3]
    cmp x8, #31
    b.ne pair_retry
    cmp x9, #32
    b.eq done
pair_retry:
    subs w12, w12, #1
    b.ne pair
    mov w0, #5
    b exit
done:
    mov w0, #0
    b exit
bad_single_register: mov w0, #1; b exit
bad_consumption: mov w0, #3; b exit
bad_pair_register: mov w0, #4
exit:
    mov x8, #93
    svc #0
.data
.balign 16
value: .quad 0, 0
.section .note.GNU-stack,"",@progbits
