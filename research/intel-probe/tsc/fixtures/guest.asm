; Bare x86-64 ring3 TSC mediation. No Linux/IRQ/timers/devices.
BITS 64
ORG 0x8000
    jmp near boot
    times 8-($-$$) db 0
    dq 0x5345454454534331
    dq rdtsc_pc, rdtscp_pc, prefixed_pc, user_cli_pc
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
gp_handler:                    ; CPU pushed error,RIP,CS,flags,RSP,SS
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
    mov rax, [rsp + 128]
    mov [0x4008], rax
    mov rcx, [rsp + 136]
    mov [0x4010], rcx
    mov rdx, [rsp + 120]
    mov [0x4018], rdx
    test rdx, rdx
    jnz reject
    and ecx, 3
    cmp ecx, 3
    jne reject
    cmp word [rax], 0x310f
    je is_rdtsc
    cmp word [rax], 0x010f
    jne maybe_prefix
    cmp byte [rax + 2], 0xf9
    jne reject
    mov eax, 2
    mov ebx, 3
    jmp request
maybe_prefix:
    cmp byte [rax], 0x66
    jne reject
    cmp word [rax + 1], 0x310f
    jne reject
    mov eax, 3
    mov ebx, 3
    jmp request
is_rdtsc:
    mov eax, 1
    mov ebx, 2
request:
    mov [0x4000], rax
    mov [0x4020], rbx
    mov dx, 0x505
    out dx, eax                ; actual userspace VMM boundary
kernel_pio_next:
    mov eax, dword [0x4028]     ; architectural EAX zero-extension
    mov [rsp + 112], rax
    mov edx, dword [0x402c]     ; architectural EDX zero-extension
    mov [rsp + 88], rdx
    cmp qword [0x4000], 2
    jne keep_rcx
    mov ecx, dword [0x4030]     ; RDTSCP AUX in ECX, zero-extended
    mov [rsp + 96], rcx
keep_rcx:
    mov rax, [rsp + 128]
    add rax, [0x4020]
    mov [rsp + 128], rax
    mov [0x4038], rax
    btr qword [rsp + 144], 16  ; clear fault RF artifact for emulated success
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
    add rsp, 8                 ; remove #GP error code
    iretq
reject:
    mov qword [0x4000], 0xba0d
    mov eax, 0xba0d
    mov dx, 0x505
    out dx, eax
reject_pio_next:
    hlt

    times 0x2000-($-$$) db 0x90
done_handler:                  ; DPL3 INT80 gate, not a Linux syscall
    mov eax, 0xd00d
    mov dx, 0x505
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
    cmp byte [0x40f0], 1
    je user_cli_pc
    PREPARE
rdtsc_pc:
    rdtsc
    OBSERVE 0x4400
    PREPARE
rdtscp_pc:
    rdtscp
    OBSERVE 0x4480
    PREPARE
prefixed_pc:
    db 0x66, 0x0f, 0x31
    OBSERVE 0x4500
    int 0x80
    ud2                        ; completion gate must never return to user
user_cli_pc:
    cli                        ; unrelated #GP(0), same error code as TSC
    int 0x80                   ; missed rejection must fail host control
    ud2
