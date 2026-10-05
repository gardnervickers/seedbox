bits 64
global _start
section .text
_start:
    mov eax, 41
    mov edi, 2
    mov esi, 2
    xor edx, edx
    syscall
    mov r12, rax
    mov eax, 49
    mov rdi, r12
    lea rsi, [rel local_address]
    mov edx, 16
    syscall
    mov eax, 45
    mov rdi, r12
    lea rsi, [rel buffer]
    mov edx, 32
    xor r10d, r10d
    lea r8, [rel peer_address]
    lea r9, [rel peer_length]
    syscall
    mov rdx, rax
    mov eax, 1
    mov edi, 1
    lea rsi, [rel buffer]
    syscall
    mov eax, 44
    mov rdi, r12
    lea rsi, [rel message]
    mov edx, 4
    xor r10d, r10d
    lea r8, [rel peer_address]
    mov r9d, 16
    syscall
    mov eax, 60
    xor edi, edi
    syscall
section .rodata
local_address: dw 2, 0x2823
    db 0, 0, 0, 0
    dq 0
message: db "pong"
section .data
peer_length: dd 16
section .bss
peer_address: resb 16
buffer: resb 32
