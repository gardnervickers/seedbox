/* Full-state entry independent of the translator's context reconstruction.
 * C supplies patterns; only SP, LR and TLS are filled from actual app state. */
.text
.global parked_enter
.type parked_enter, %function
parked_enter:
    sub sp, sp, #256
    stp x19, x20, [sp, #0]
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]
    stp q8, q9, [sp, #96]
    stp q10, q11, [sp, #128]
    stp q12, q13, [sp, #160]
    stp q14, q15, [sp, #192]
    mrs x1, fpsr
    mrs x2, fpcr
    stp x1, x2, [sp, #224]
    str x0, [sp, #240]
    mov x30, x0
    mov x1, sp
    str x1, [x30, #248]
    adr x1, parked_return
    str x1, [x30, #240]
    mrs x1, tpidr_el0
    str x1, [x30, #280]
    ldr x1, [x30, #264]
    msr fpsr, x1
    ldr x1, [x30, #272]
    msr fpcr, x1
    ldr x1, [x30, #256]
    msr nzcv, x1
    ldp q0, q1, [x30, #288]
    ldp q2, q3, [x30, #320]
    ldp q4, q5, [x30, #352]
    ldp q6, q7, [x30, #384]
    ldp q8, q9, [x30, #416]
    ldp q10, q11, [x30, #448]
    ldp q12, q13, [x30, #480]
    ldp q14, q15, [x30, #512]
    ldp q16, q17, [x30, #544]
    ldp q18, q19, [x30, #576]
    ldp q20, q21, [x30, #608]
    ldp q22, q23, [x30, #640]
    ldp q24, q25, [x30, #672]
    ldp q26, q27, [x30, #704]
    ldp q28, q29, [x30, #736]
    ldp q30, q31, [x30, #768]
    ldp x0, x1, [x30, #0]
    ldp x2, x3, [x30, #16]
    ldp x4, x5, [x30, #32]
    ldp x6, x7, [x30, #48]
    ldp x8, x9, [x30, #64]
    ldp x10, x11, [x30, #80]
    ldp x12, x13, [x30, #96]
    ldp x14, x15, [x30, #112]
    ldp x16, x17, [x30, #128]
    ldp x18, x19, [x30, #144]
    ldp x20, x21, [x30, #160]
    ldp x22, x23, [x30, #176]
    ldp x24, x25, [x30, #192]
    ldp x26, x27, [x30, #208]
    ldp x28, x29, [x30, #224]
    ldr x30, [x30, #240]
    br x16
parked_return:
    str x0, [sp, #248]
    ldp x1, x2, [sp, #224]
    msr fpsr, x1
    msr fpcr, x2
    ldp q8, q9, [sp, #96]
    ldp q10, q11, [sp, #128]
    ldp q12, q13, [sp, #160]
    ldp q14, q15, [sp, #192]
    ldp x19, x20, [sp, #0]
    ldp x21, x22, [sp, #16]
    ldp x23, x24, [sp, #32]
    ldp x25, x26, [sp, #48]
    ldp x27, x28, [sp, #64]
    ldp x29, x30, [sp, #80]
    ldr x0, [sp, #248]
    add sp, sp, #256
    ret
.size parked_enter, .-parked_enter
.section .note.GNU-stack,"",%progbits
