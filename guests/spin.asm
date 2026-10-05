bits 64
global _start
section .text
_start:
    inc rax
    jmp _start
