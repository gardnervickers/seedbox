/* Application instruction semantics only; no DR private state or guest ABI. */
.text
.globl probe_begin, probe_end, register_probe, signal_probe, signal_fault, signal_resume
probe_begin:
register_probe:
#ifdef __x86_64__
    mov $17, %rax
    mov $23, %rcx
    mov $31, %rdx
    cmp $17, %rax
    jne failed
    cmp $18, %rax
    je failed
    cmp $23, %rcx
    jne failed
    cmp $31, %rdx
    jne failed
    add %rcx, %rax
    add %rdx, %rax
    cmp $71, %rax
    jne failed
    xor %eax, %eax
    ret
signal_probe:
    pcmpeqb %xmm0, %xmm0
    mov $17, %rax
signal_fault:
    ud2
signal_resume:
    cmp $42, %rax
    jne failed
    pmovmskb %xmm0, %eax
    cmp $65535, %eax
    jne failed
    xor %eax, %eax
    ret
failed:
    mov $1, %eax
    ret
#else
    mov x0, #17
    mov x1, #23
    mov x2, #31
    cmp x0, #17
    b.ne failed
    cmp x0, #18
    b.eq failed
    cmp x1, #23
    b.ne failed
    cmp x2, #31
    b.ne failed
    add x0, x0, x1
    add x0, x0, x2
    cmp x0, #71
    b.ne failed
    mov x0, #0
    ret
signal_probe:
    movi v0.16b, #255
    mov x0, #17
signal_fault:
    .inst 0x00000000
signal_resume:
    cmp x0, #42
    b.ne failed
    uminv b1, v0.16b
    umov w0, v1.b[0]
    cmp x0, #255
    b.ne failed
    mov x0, #0
    ret
.globl exclusive_probe
exclusive_probe:
    sub sp, sp, #16
    mov x1, sp
    mov x2, #73
    str x2, [x1]
1:  ldxr x0, [x1]
    add x0, x0, #1
    stxr w2, x0, [x1]
    cbnz w2, 1b
    ldr x0, [x1]
    cmp x0, #74
    b.ne exclusive_failed
    /* Architectural load/base overlap: preserve the address before x0 is
     * overwritten with the loaded value. The store uses an independent base. */
    mov x0, x1
2:  ldxr x0, [x0]
    stxr w2, x0, [x1]
    cbz w2, 3f
    mov x0, x1
    b 2b
3:  ldr x0, [x1]
    cmp x0, #74
    b.ne exclusive_failed
    mov x0, #0
    add sp, sp, #16
    ret
exclusive_failed:
    add sp, sp, #16
failed:
    mov x0, #1
    ret
#endif
probe_end:
.section .note.GNU-stack,"",%progbits
