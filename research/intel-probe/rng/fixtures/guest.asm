; Bare Intel original-opcode #UD RNG mediation; no Linux.
; AMD fixture smoke mode alone rewrites these three known sites to UD2.
BITS 64
ORG 0x8000
    jmp near boot
    times 8-($-$$) db 0
    dq 0x53454544524e4731
    dq rng16_pc, rng32_pc, rng64_pc, unrelated_pc
    dq kernel_pio_next, reject_pio_next, done_pio_next
    times 0x80-($-$$) db 0
boot:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    push qword 0x23
    push qword 0x30000
    push qword 2                ; IF0, IOPL0
    push qword 0x1b
    push qword user
    iretq

    times 0x1000-($-$$) db 0x90
ud_handler:                    ; CPU pushed RIP,CS,flags,RSP,SS; no error code
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    inc qword [0x4080]
    mov rax, [rsp + 120]
    mov [0x4008], rax
    mov rcx, [rsp + 128]
    mov [0x4010], rcx
    and ecx, 3
    cmp ecx, 3
    jne reject
    cmp rax, rng16_pc
    je kind16
    cmp rax, rng32_pc
    je kind32
    cmp rax, rng64_pc
    jne reject
    cmp byte [0x40f1], 1
    je check64_rewrite
    cmp dword [rax], 0xf0c70f48 ; original RDRAND RAX
    jne reject
    jmp valid64
check64_rewrite:
    cmp dword [rax], 0x90900b0f
    jne reject
valid64:
    mov eax, 3
    mov ebx, 4
    jmp request
kind16:
    cmp byte [0x40f1], 1
    je check16_rewrite
    cmp dword [rax], 0xf0c70f66 ; original RDRAND AX
    jne reject
    jmp valid16
check16_rewrite:
    cmp dword [rax], 0x90900b0f
    jne reject
valid16:
    mov eax, 1
    mov ebx, 4
    jmp request
kind32:
    cmp byte [0x40f1], 1
    je check32_rewrite
    cmp word [rax], 0xc70f
    jne reject
    cmp byte [rax+2], 0xf8 ; original RDSEED EAX
    jne reject
    jmp valid32
check32_rewrite:
    cmp word [rax], 0x0b0f
    jne reject
    cmp byte [rax+2], 0x90
    jne reject
valid32:
    mov eax, 2
    mov ebx, 3
request:
    mov [0x4000], rax
    mov [0x4020], rbx
    mov dx, 0x506
    out dx, eax
kernel_pio_next:
    mov rax, [0x4028]
    cmp qword [0x4000], 1
    jne not16
    mov rcx, [rsp+112]
    and rcx, -65536
    and eax, 0xffff
    or rax, rcx
    jmp save_result
not16:
    cmp qword [0x4000], 2
    jne save_result
    mov eax, eax           ; architectural EAX zero-extension
save_result:
    mov [rsp+112], rax
    mov rax, [rsp+120]
    add rax, [0x4020]
    mov [rsp+120], rax
    mov [0x4038], rax
    ; Successful RNG sets CF, clears OF/SF/ZF/AF/PF. Preserve other flags.
    and qword [rsp+136], ~0x108d5 ; clear fault RF plus affected flags
    or qword [rsp+136], 1
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    iretq
reject:
    mov qword [0x4000], 0xba0d
    mov eax, 0xba0d
    mov dx, 0x506
    out dx, eax
reject_pio_next:
    hlt

    times 0x2000-($-$$) db 0x90
done_handler:                  ; DPL3 INT80 gate, not a Linux syscall
    mov eax, 0xd00d
    mov dx, 0x506
    out dx, eax
done_pio_next:
    hlt

%macro PREPARE 0
    mov rax, -1
    mov rdx, 0xffffffff00000000
    mov rcx, 0xcafebabe76543210
    mov rbx, 0x1111222233334444
    mov rsi, 0x2222333344445555
    mov rdi, 0x3333444455556666
    mov rbp, 0x4444555566667777
    mov r8,  0x5555666677778888
    mov r9,  0x6666777788889999
    mov r10, 0x777788889999aaaa
    mov r13, 0x88889999aaaabbbb
    mov r14, 0x9999aaaabbbbcccc
    mov r15, 0xaaaabbbbccccdddd
    push qword 0x8d7           ; selected arithmetic flags, IF0/IOPL0
    popfq
    pushfq
    pop r11
%endmacro
%macro OBSERVE 1
    pushfq
    pop r12
    mov [%1 + 0], rax
    mov [%1 + 8], rbx
    mov [%1 + 16], rcx
    mov [%1 + 24], rdx
    mov [%1 + 32], rsi
    mov [%1 + 40], rdi
    mov [%1 + 48], rbp
    mov [%1 + 56], r8
    mov [%1 + 64], r9
    mov [%1 + 72], r10
    mov [%1 + 80], r11
    mov [%1 + 88], r12
    mov [%1 + 96], r13
    mov [%1 + 104], r14
    mov [%1 + 112], r15
    mov [%1 + 120], rsp
%endmacro
    times 0x8000-($-$$) db 0x90
user:
    mov eax, 1
    cpuid
    and ecx, 1 << 30
    mov [0x40a0], rcx
    mov eax, 7
    xor ecx, ecx
    cpuid
    and ebx, 1 << 18
    mov [0x40a8], rbx
    cmp byte [0x40f0], 1
    je unrelated_pc
    PREPARE
    nop
    nop                     ; portable fixture may replace these with INT 6
rng16_pc:
    rdrand ax               ; 66 0f c7 f0, replaced by UD2+2 NOP
    OBSERVE 0x4400
    PREPARE
    nop
    nop
rng32_pc:
    rdseed eax              ; 0f c7 f8, replaced by UD2+NOP
    OBSERVE 0x4480
    PREPARE
    nop
    nop
rng64_pc:
    rdrand rax              ; 48 0f c7 f0, replaced by UD2+2 NOP
    OBSERVE 0x4500
    int 0x80
    ud2
unrelated_pc:
    ud2                    ; ordinary #UD at an unmanifested PC must reject
    int 0x80
    ud2
