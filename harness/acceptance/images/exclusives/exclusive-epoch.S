// Static AArch64 regression for the translated Seedbox guest. Linux may
// fail an otherwise valid exclusive store spuriously, so success cases retry.
// Exit codes: 1/57 TPIDRRO_EL0; 2-3 consumed reservation; 4-5 CLREX;
// 6-7 syscall; 8-10 address mismatch; 36-37 syscall/ABA.
// Each three-code success range
// reports a bad load, exhausted retries, or a bad stored value, respectively.
.global _start
.text

.macro require_equal left, right, code
    cmp \left, \right
    b.eq 1f
    mov w0, #\code
    b exit
1:
.endm

.macro require_failed status, code
    cbnz \status, 1f
    mov w0, #\code
    b exit
1:
.endm

// Data and result are matching W or X registers; base/status are overridable.
.macro single_case load, store, read, result, data, initial, replacement, code, base=x3, status=w2
    mov x1, #\initial
    str x1, [\base]
    mov w12, #1000
2:
    mov \data, #\initial
    \load \result, [\base]
    require_equal \result, \data, \code
    mov \data, #\replacement
    \store \status, \data, [\base]
    cbz \status, 3f
    subs w12, w12, #1
    b.ne 2b
    mov w0, #(\code + 1)
    b exit
3:
    \read \result, [\base]
    require_equal \result, \data, (\code + 2)
.endm

// Keep the address and all data/status registers distinct. Exercise x0-x3
// as live operands, with both a low data register and a low status register.
.macro pair_case load, store, first, second, status, scratch, code
    mov \first, #11
    mov \second, #12
    stp \first, \second, [x3]
    mov w12, #1000
2:
    \load \first, \second, [x3]
    mov \scratch, #11
    require_equal \first, \scratch, \code
    mov \scratch, #12
    require_equal \second, \scratch, \code
    mov \first, #31
    mov \second, #32
    \store \status, \first, \second, [x3]
    cbz \status, 3f
    subs w12, w12, #1
    b.ne 2b
    mov w0, #(\code + 1)
    b exit
3:
    ldp \first, \second, [x3]
    mov \scratch, #31
    require_equal \first, \scratch, (\code + 2)
    mov \scratch, #32
    require_equal \second, \scratch, (\code + 2)
.endm

_start:
    mrs x0, tpidrro_el0
    mov x1, #0
    require_equal x0, x1, 1
    adrp x3, value
    add x3, x3, :lo12:value

    // Codes 12-14: ordinary 64-bit load/store, followed by consumption.
    single_case ldxr, stxr, ldr, x0, x1, 0x123, 0x234, 12
    mov x1, #0x345
    stxr w2, x1, [x3]
    require_failed w2, 2
    ldr x0, [x3]
    mov x1, #0x234
    require_equal x0, x1, 3

    mov x1, #0x456
    str x1, [x3]
    ldxr x0, [x3]
    clrex
    mov x1, #0x567
    stxr w2, x1, [x3]
    require_failed w2, 4
    ldr x0, [x3]
    mov x1, #0x456
    require_equal x0, x1, 5

    // A syscall alone invalidates the reservation, with no intervening store.
    mov x1, #0x678
    str x1, [x3]
    ldxr x0, [x3]
    mov x8, #172                 // getpid
    svc #0
    mov x1, #0x89a
    stxr w2, x1, [x3]
    require_failed w2, 6
    ldr x0, [x3]
    mov x1, #0x678
    require_equal x0, x1, 7

    // Restore the loaded value after an ABA, then cross an explicit syscall
    // boundary. Value equality must not revive the old exclusive reservation.
    ldxr x0, [x3]
    mov x1, #0x789
    str x1, [x3]
    mov x1, #0x678
    str x1, [x3]
    mov x8, #172
    svc #0
    mov x1, #0x89a
    stxr w2, x1, [x3]
    require_failed w2, 36
    ldr x0, [x3]
    mov x1, #0x678
    require_equal x0, x1, 37

    // Separate pages keep the two addresses in different reservation granules.
    adrp x4, other
    add x4, x4, :lo12:other
    mov x1, #0xabc
    str x1, [x4]
    ldxr x0, [x3]
    mov x1, #0x99
    stxr w2, x1, [x4]
    require_failed w2, 8
    ldr x0, [x3]
    mov x1, #0x678
    require_equal x0, x1, 9
    ldr x0, [x4]
    mov x1, #0xabc
    require_equal x0, x1, 10

    // Codes 15-35 cover normal/acquire-release word, byte, halfword, and X.
    single_case ldxr, stxr, ldr, w0, w1, 0x321, 0x432, 15
    single_case ldaxr, stlxr, ldr, w0, w1, 0x432, 0x543, 18
    single_case ldxrb, stxrb, ldrb, w0, w1, 0x12, 0xab, 21
    single_case ldaxrb, stlxrb, ldrb, w0, w1, 0x23, 0xbc, 24
    single_case ldxrh, stxrh, ldrh, w0, w1, 0x321, 0x7654, 27
    single_case ldaxrh, stlxrh, ldrh, w0, w1, 0x432, 0x8765, 30
    single_case ldaxr, stlxr, ldr, x0, x1, 0x543, 0x654, 33

    // Codes 45-56: both pair widths and acquire/release orderings.
    pair_case ldxp, stxp, x0, x1, w2, x4, 45
    pair_case ldaxp, stlxp, x1, x2, w0, x4, 48
    pair_case ldxp, stxp, w0, w1, w2, w4, 51
    pair_case ldaxp, stlxp, w1, w2, w0, w4, 54

    // Codes 58-66: DynamoRIO steals x28, but each application operand must
    // still refer to its application value, including a W28 status write.
    mov x28, x3
    single_case ldxr, stxr, ldr, x0, x1, 0x123, 0x234, 58, x28
    single_case ldaxr, stlxr, ldr, x28, x1, 0x345, 0x456, 61
    movz x28, #0xface, lsl #48
    single_case ldxr, stxr, ldr, x0, x1, 0x567, 0x678, 64, x3, w28
    mov x0, #0
    require_equal x28, x0, 74     // W28 status must also zero the upper bits.

    // Codes 67-69: SP denotes an address here, while register 31 as a load
    // destination below denotes XZR/WZR. Keep SP aligned and restore it.
    mov x14, sp
    add x15, x3, #128
    mov sp, x15
    single_case ldaxr, stlxr, ldr, x0, x1, 0x789, 0x89a, 67, sp
    mov sp, x14

    // Codes 70-73: a discarded load still establishes the reservation.
    mov x1, #0x9ab
    str x1, [x3]
    mov x1, #0xabc
    mov w12, #1000
discard_x:
    ldxr xzr, [x3]
    stxr w2, x1, [x3]
    cbz w2, discard_x_stored
    subs w12, w12, #1
    b.ne discard_x
    mov w0, #70
    b exit
discard_x_stored:
    ldr x0, [x3]
    require_equal x0, x1, 71

    mov w1, #0xbcd
    str w1, [x3]
    mov w1, #0xcde
    mov w12, #1000
discard_w:
    ldaxr wzr, [x3]
    stlxr w2, w1, [x3]
    cbz w2, discard_w_stored
    subs w12, w12, #1
    b.ne discard_w
    mov w0, #72
    b exit
discard_w_stored:
    ldr w0, [x3]
    require_equal w0, w1, 73

    mrs x0, tpidrro_el0
    mov x1, #0
    require_equal x0, x1, 57
    mov w0, #0
exit:
    mov x8, #93                  // exit
    svc #0

.data
.balign 4096
value: .quad 0, 0
.balign 4096
other: .quad 0

.section .note.GNU-stack,"",@progbits
