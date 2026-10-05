bits 64
global _start
section .text
_start:
    mov eax,41
    mov edi,2
    mov esi,1
    xor edx,edx
    syscall
    mov r12,rax
    mov eax,49
    mov rdi,r12
    lea rsi,[rel local_address]
    mov edx,16
    syscall
    test rax,rax
    jnz fail
    mov eax,50
    mov rdi,r12
    mov esi,16
    syscall
    mov eax,291
    xor edi,edi
    syscall
    mov r13,rax
    mov eax,233
    mov rdi,r13
    mov esi,1
    mov rdx,r12
    lea r10,[rel interest]
    syscall
    call wait_ready
    mov eax,288
    mov rdi,r12
    xor esi,esi
    xor edx,edx
    mov r10d,0x800
    syscall
    test rax,rax
    js fail
    mov r14,rax
    mov eax,233
    mov rdi,r13
    mov esi,2
    mov rdx,r12
    xor r10d,r10d
    syscall
    mov eax,233
    mov rdi,r13
    mov esi,1
    mov rdx,r14
    lea r10,[rel interest]
    syscall
    xor ebx,ebx
.read:
    call wait_ready
    mov eax,0
    mov rdi,r14
    lea rsi,[rel buffer]
    add rsi,rbx
    mov edx,4
    sub rdx,rbx
    syscall
    cmp rax,-11
    je .read
    test rax,rax
    jle fail
    add rbx,rax
    cmp rbx,4
    jb .read
    cmp dword [rel buffer],'ping'
    jne fail
    mov eax,1
    mov edi,1
    lea rsi,[rel buffer]
    mov edx,4
    syscall
    mov eax,1
    mov rdi,r14
    lea rsi,[rel reply]
    mov edx,4
    syscall
    cmp rax,4
    jne fail
    ; Leaving the socket open tests exit_group's implicit FIN.
    mov eax,231
    xor edi,edi
    syscall
wait_ready:
    mov eax,281
    mov rdi,r13
    lea rsi,[rel event]
    mov edx,1
    mov r10d,1000
    xor r8d,r8d
    mov r9d,8
    syscall
    cmp rax,1
    jne fail
    cmp qword [rel event+4],0x42
    jne fail
    ret
fail:
    mov eax,231
    mov edi,1
    syscall
section .rodata
local_address: dw 2,0x2823
    db 10,0,0,2
    dq 0
interest: dd 1
    dq 0x42
reply: db 'pong'
section .bss
event: resb 12
buffer: resb 4
