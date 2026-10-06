BITS 64
ORG 0x8000
 dq loop_pc, loop_branch, exits_loop, rep_pc, uncond_pc, done_pc, user_loop
 dq exits_pio_next, exits_canary_next
 times 0x80-($-$$) db 0
boot:
 mov dx, 0x505
 mov eax, 1
 out dx,eax
 hlt
 times 0x1000-($-$$) db 0x90
int_handler:
 mov dx,0x505
 mov eax,2
 out dx,eax
 iretq
 times 0x2000-($-$$) db 0x90
plain:
 mov ecx, [0x4000]
 xor r8,r8
loop_pc:
 add r8,7
loop_branch:
 dec ecx
 jnz loop_pc
 mov dx,0x505
 mov eax,9
 out dx,eax
done_pc:
 hlt
 times 0x3000-($-$$) db 0x90
exits:
 mov ecx,[0x4000]
 xor r8,r8
exits_loop:
 add r8,7
 mov dx,0x505
 mov eax,3
 out dx,eax
exits_pio_next:
 inc dword [0x4100]
exits_canary_next:
 dec ecx
 jnz exits_loop
 mov dx,0x505
 mov eax,9
 out dx,eax
 hlt
 times 0x4000-($-$$) db 0x90
rep_fixture:
 mov ecx,[0x4000]
.prep:
 dec ecx
 jnz .prep
 mov ecx,8192
 mov rsi,0x50000
 mov rdi,0x60000
rep_pc:
 rep movsb
 mov dx,0x505
 mov eax,9
 out dx,eax
 hlt
 times 0x5000-($-$$) db 0x90
unconditional:
 xor r8,r8
uncond_pc:
 inc r8
 jmp uncond_pc
 times 0x6000-($-$$) db 0x90
user_entry:
 push qword 0x23
 push qword 0x70000
 push qword 2
 push qword 0x1b
 push qword user_loop_init
 iretq
user_loop_init:
 mov ecx,[0x4000]
 xor r8,r8
user_loop:
 add r8,7
 dec ecx
 jnz user_loop
 int 128
 hlt
