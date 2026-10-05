bits 64
global _start
section .text
_start:
    mov eax, 1
    mov edi, 1
    lea rsi, [rel message]
    mov edx, 6
    syscall
    mov eax, 60
    xor edi, edi
    syscall
section .rodata
message: db "hello", 10
