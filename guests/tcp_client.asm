bits 64
global _start
section .text
_start:
    mov eax,35
    lea rdi,[rel pause]
    xor esi,esi
    syscall
    mov eax,41
    mov edi,2
    mov esi,1
    xor edx,edx
    syscall
    mov r12,rax
    mov eax,42
    mov rdi,r12
    lea rsi,[rel remote]
    mov edx,16
    syscall
    test rax,rax
    jnz fail
    mov eax,1
    mov rdi,r12
    lea rsi,[rel message]
    mov edx,2
    syscall
    cmp rax,2
    jne fail
    mov eax,1
    mov rdi,r12
    lea rsi,[rel message+2]
    mov edx,2
    syscall
    xor ebx,ebx
.read:
    mov eax,0
    mov rdi,r12
    lea rsi,[rel buffer]
    add rsi,rbx
    mov edx,2
    syscall
    test rax,rax
    jle fail
    add rbx,rax
    cmp rbx,4
    jb .read
    cmp dword [rel buffer],'pong'
    jne fail
    mov eax,0
    mov rdi,r12
    lea rsi,[rel buffer+4]
    mov edx,1
    syscall
    test rax,rax
    jnz fail
    mov eax,1
    mov edi,1
    lea rsi,[rel buffer]
    mov edx,4
    syscall
    mov eax,60
    xor edi,edi
    syscall
fail:
    mov eax,60
    mov edi,1
    syscall
section .rodata
pause: dq 0,1000000
remote: dw 2,0x2823
    db 10,0,0,2
    dq 0
message: db 'ping'
section .bss
buffer: resb 5
