; The unsynchronized build is a deliberate negative control. ATOMIC is its fix.
bits 64
global _start
section .text
_start:
    mov eax, 56
    mov edi, 0x50f00 ; CLONE_VM|FS|FILES|SIGHAND|THREAD|SYSVSEM
    lea rsi, [rel child_stack_end]
    xor edx, edx
    xor r10d, r10d
    xor r8d, r8d
    syscall
    test rax, rax
    jz child
    call increment
wait_done:
    cmp dword [rel done], 1
    je print_counter
    mov eax, 202
    lea rdi, [rel done]
    mov esi, 128 ; FUTEX_WAIT_PRIVATE
    xor edx, edx
    xor r10d, r10d
    xor r8d, r8d
    xor r9d, r9d
    syscall
    jmp wait_done
print_counter:
    mov eax, 1
    mov edi, 1
    lea rsi, [rel counter]
    mov edx, 4
    syscall
    mov eax, 231
    xor edi, edi
    syscall
child:
    call increment
    mov dword [rel done], 1
    mov eax, 202
    lea rdi, [rel done]
    mov esi, 129 ; FUTEX_WAKE_PRIVATE
    mov edx, 1
    xor r10d, r10d
    xor r8d, r8d
    xor r9d, r9d
    syscall
    mov eax, 60
    xor edi, edi
    syscall
increment:
    mov ecx, 32
.again:
%ifdef ATOMIC
    lock inc dword [rel counter]
%else
    mov eax, [rel counter]
    inc eax
    mov [rel counter], eax
%endif
    dec ecx
    jnz .again
    ret
section .bss
align 16
counter: resd 1
done: resd 1
child_stack: resb 65536
child_stack_end:
