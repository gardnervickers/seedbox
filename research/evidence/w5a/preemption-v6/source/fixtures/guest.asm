BITS 64
ORG 0x8000
 times 0x80-($-$$) db 0
boot:
 mov dx, 0x505
 mov eax, 1
 out dx, eax
 ; Host chooses RIP after this completed boundary.
 hlt
 times 0x1000-($-$$) db 0x90
int_handler:
 mov dx, 0x505
 mov eax, 2
 out dx, eax
 iretq
 times 0x2000-($-$$) db 0x90
cpl0_branches:
 mov ecx, [0x4000]
 xor eax, eax
.loop:
 add eax, 7
 dec ecx
 jnz .loop
 mov [0x4020], eax
 mov dx, 0x505
 mov eax, 3
 out dx, eax
 hlt
 times 0x3000-($-$$) db 0x90
cpl0_extra:
 mov ecx, [0x4000]
 xor eax, eax
.loop:
 add eax, 7
 test eax, eax
 jz .skip
.skip:
 dec ecx
 jnz .loop
 mov [0x4020], eax
 mov dx, 0x505
 mov eax, 3
 out dx, eax
 hlt
 times 0x4000-($-$$) db 0x90
cpl0_nonbranch:
 xor eax, eax
 times 50000 add eax, 7
 mov [0x4020], eax
 mov dx, 0x505
 mov eax, 3
 out dx, eax
 hlt


 times 0x30000-($-$$) db 0x90
cpl3_entry:
 push qword 0x23
 push qword 0x38000
 push qword 2
 push qword 0x1b
 push qword user_loop
 iretq
user_loop:
 mov ecx, [0x4000]
 xor eax, eax
.loop:
 add eax, 7
 dec ecx
 jnz .loop
 mov [0x4020], eax
 int 128
 hlt
 times 0x31000-($-$$) db 0x90
locked_loop:
 mov ecx, [0x4000]
.loop:
 lock add dword [0x4020], 1
 dec ecx
 jnz .loop
 mov dx, 0x505
 mov eax, 3
 out dx, eax
 hlt
