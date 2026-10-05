bits 64
global _start
section .text
_start:
    mov eax, 257
    mov rdi, -100
    lea rsi, [rel path]
    mov edx, 0x242 ; O_RDWR|O_CREAT|O_TRUNC
    mov r10d, 0o600
    syscall
    mov r12, rax
    mov eax, 1
    mov rdi, r12
    lea rsi, [rel contents]
    mov edx, 5
    syscall
    mov eax, 74
    mov rdi, r12
    syscall
    mov eax, 8
    mov rdi, r12
    xor esi, esi
    xor edx, edx
    syscall
    xor eax, eax
    mov rdi, r12
    lea rsi, [rel buffer]
    mov edx, 5
    syscall
    mov eax, 1
    mov edi, 1
    lea rsi, [rel buffer]
    mov edx, 5
    syscall
    mov eax, 318
    lea rdi, [rel random]
    mov esi, 8
    xor edx, edx
    syscall
    mov eax, 1
    mov edi, 1
    lea rsi, [rel random]
    mov edx, 8
    syscall
    mov eax, 228
    mov edi, 1
    lea rsi, [rel clock_values]
    syscall
    mov eax, 35
    lea rdi, [rel sleep]
    xor esi, esi
    syscall
    mov eax, 228
    mov edi, 1
    lea rsi, [rel clock_values + 16]
    syscall
    mov eax, 1
    mov edi, 1
    lea rsi, [rel clock_values]
    mov edx, 32
    syscall
    mov eax, 60
    xor edi, edi
    syscall
section .rodata
path: db "/state", 0
contents: db "state"
sleep: dq 1, 0
section .bss
buffer: resb 16
random: resb 8
clock_values: resb 32
