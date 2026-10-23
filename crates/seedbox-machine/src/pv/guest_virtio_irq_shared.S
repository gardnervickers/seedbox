/* Raw x86 PV fixture: snapshot inside a masked handler, then deliver a
 * newly asserted NIC IRQ at the first interruptible instruction after unmask. */
.text
.globl _start
_start:
    cli
    mov $0x0a010000, %ebx
    mov $3, %eax
    mov %rax, 32(%rbx)
    mov $0x11, %al
    out %al, $0x20
    mov $0x30, %al
    out %al, $0x21
    mov $4, %al
    out %al, $0x21
    mov $1, %al
    out %al, $0x21
    lea handler(%rip), %rax
    lea gate(%rip), %rdi
    mov %ax, (%rdi)
    shr $16, %rax
    mov %ax, 6(%rdi)
    shr $16, %rax
    mov %eax, 8(%rdi)
    lea idt(%rip), %rax
    mov %rax, idtr+2(%rip)
    lidt idtr(%rip)
    mov $0x0a000000, %ecx
    movq $0x1007f, 56(%rcx)
    mov $0x502, %dx
    mov $1, %al
    out %al, %dx
    sti
    nop
idle:
    hlt
    /* First instruction at the saved return RIP. The repeated IRQ must
     * interrupt before this store, even if a later HLT has the same RIP. */
    movq $1, resumed(%rip)
    jmp idle
handler:
    incq count(%rip)
    movq $0x180ff, 56(%rcx)
    cmpq $1, count(%rip)
    jne second
    /* ACK the actual published NIC status, then stop with IF=0/masked. */
    call ack

    mov $0x501, %dx
    mov $1, %al
    out %al, %dx
    movq $0x1007f, 56(%rcx)
    mov $0x502, %dx
    out %al, %dx
    iretq
second:
    cmpq $0, resumed(%rip)
    jne late_delivery
    call ack
    movq $1, 40(%rbx)
late_delivery:
    ud2
ack:
    mov 80(%rcx), %rax
    cmp $1, %rax
    jne late_delivery
    mov %rax, %rsi
    shl $32, %rsi
    or %rsi, %rax
    mov %rax, 80(%rcx)
    ret
.balign 8
count: .quad 0
resumed: .quad 0
idtr: .word 0x37 * 16 + 15
      .quad 0
.balign 16
idt: .zero 0x37 * 16
gate: .word 0, 0x10
      .byte 0, 0x8e
      .word 0
      .long 0, 0
