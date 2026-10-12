/* Full-state entry independent of the translator's context reconstruction.
 * C supplies patterns; only SP, LR and TLS are filled from actual app state. */
.text
.global parked_enter
.type parked_enter, %function
parked_enter:
    sub sp, sp, #272
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
    stp x0, x1, [sp, #240]
    mrs x1, fpsr
    mrs x2, fpcr
    stp x1, x2, [sp, #224]
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
    str x0, [sp, #256]
    stp x16, x17, [sp, #-16]!
    ldr x16, [sp, #264]
    stp x0, x1, [x16, #0]
    stp x2, x3, [x16, #16]
    stp x4, x5, [x16, #32]
    stp x6, x7, [x16, #48]
    stp x8, x9, [x16, #64]
    stp x10, x11, [x16, #80]
    stp x12, x13, [x16, #96]
    stp x14, x15, [x16, #112]
    stp x18, x19, [x16, #144]
    stp x20, x21, [x16, #160]
    stp x22, x23, [x16, #176]
    stp x24, x25, [x16, #192]
    stp x26, x27, [x16, #208]
    stp x28, x29, [x16, #224]
    str x30, [x16, #240]
    ldp x0, x1, [sp]
    stp x0, x1, [x16, #128]
    add x0, sp, #16
    str x0, [x16, #248]
    mrs x0, nzcv
    str x0, [x16, #256]
    mrs x0, fpsr
    str x0, [x16, #264]
    mrs x0, fpcr
    str x0, [x16, #272]
    mrs x0, tpidr_el0
    str x0, [x16, #280]
    stp q0, q1, [x16, #288]
    stp q2, q3, [x16, #320]
    stp q4, q5, [x16, #352]
    stp q6, q7, [x16, #384]
    stp q8, q9, [x16, #416]
    stp q10, q11, [x16, #448]
    stp q12, q13, [x16, #480]
    stp q14, q15, [x16, #512]
    stp q16, q17, [x16, #544]
    stp q18, q19, [x16, #576]
    stp q20, q21, [x16, #608]
    stp q22, q23, [x16, #640]
    stp q24, q25, [x16, #672]
    stp q26, q27, [x16, #704]
    stp q28, q29, [x16, #736]
    stp q30, q31, [x16, #768]
    add sp, sp, #16
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
    ldr x0, [sp, #256]
    add sp, sp, #272
    ret
.size parked_enter, .-parked_enter
.section .note.GNU-stack,"",%progbits
