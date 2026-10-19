/* **********************************************************
 * Copyright (c) 2018-2026 Google, Inc.  All rights reserved.
 * Copyright (c) 2025 Arm Limited  All rights reserved.
 * **********************************************************/

/*
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * * Redistributions of source code must retain the above copyright notice,
 *   this list of conditions and the following disclaimer.
 *
 * * Redistributions in binary form must reproduce the above copyright notice,
 *   this list of conditions and the following disclaimer in the documentation
 *   and/or other materials provided with the distribution.
 *
 * * Neither the name of VMware, Inc. nor the names of its contributors may be
 *   used to endorse or promote products derived from this software without
 *   specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL VMWARE, INC. OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
 * DAMAGE.
 */

/* Sentinel constants additionally Copyright (c) 2015-2025 Google, Inc.
 * Exact source356c1821045a4b5678d0813e132074324623df6e,
 * detach_state_shared.{c,h}; independent GNU assembly uses only application
 * registers/layout and public DR APIs, no internal asm/harness header. */
#if defined(__x86_64__)
.text
.p2align 16
.globl detach_gprs_cache
.type detach_gprs_cache,@function
detach_gprs_cache:
push %rbp
push %rbx
push %r12
push %r13
push %r14
push %r15
mov %rsp,detach_saved_stack(%rip)
movdqu detach_vector_expect+0(%rip),%xmm0
movdqu detach_vector_expect+16(%rip),%xmm1
movdqu detach_vector_expect+32(%rip),%xmm2
movdqu detach_vector_expect+48(%rip),%xmm3
movdqu detach_vector_expect+64(%rip),%xmm4
movdqu detach_vector_expect+80(%rip),%xmm5
movdqu detach_vector_expect+96(%rip),%xmm6
movdqu detach_vector_expect+112(%rip),%xmm7
movdqu detach_vector_expect+128(%rip),%xmm8
movdqu detach_vector_expect+144(%rip),%xmm9
movdqu detach_vector_expect+160(%rip),%xmm10
movdqu detach_vector_expect+176(%rip),%xmm11
movdqu detach_vector_expect+192(%rip),%xmm12
movdqu detach_vector_expect+208(%rip),%xmm13
movdqu detach_vector_expect+224(%rip),%xmm14
movdqu detach_vector_expect+240(%rip),%xmm15
movabs $0x12345678abcdef01,%rax
movabs $0x2345678abcdef012,%rcx
movabs $0x345678abcdef0123,%rdx
movabs $0x45678abcdef01234,%rbx
movabs $0x5678abcdef012345,%rbp
movabs $0x678abcdef0123456,%rsi
movabs $0x78abcdef01234567,%rdi
movabs $0x1234567890abcdef,%r8
movabs $0xf1234567890abcde,%r9
movabs $0xef1234567890abcd,%r10
movabs $0xdef1234567890abc,%r11
movabs $0xcdef1234567890ab,%r12
movabs $0xbcdef1234567890a,%r13
movabs $0xabcdef1234567890,%r14
movabs $0xabcdef123456789,%r15
movl $1,detach_ready(%rip)
.Lgprs_cache_loop:
movl $1,detach_progress(%rip)
cmpb $1,detach_released(%rip)
jne .Lgprs_cache_loop
mov %rax,detach_gpr+0(%rip)
mov %rcx,detach_gpr+8(%rip)
mov %rdx,detach_gpr+16(%rip)
mov %rbx,detach_gpr+24(%rip)
mov %rbp,detach_gpr+32(%rip)
mov %rsi,detach_gpr+40(%rip)
mov %rdi,detach_gpr+48(%rip)
mov %r8,detach_gpr+56(%rip)
mov %r9,detach_gpr+64(%rip)
mov %r10,detach_gpr+72(%rip)
mov %r11,detach_gpr+80(%rip)
mov %r12,detach_gpr+88(%rip)
mov %r13,detach_gpr+96(%rip)
mov %r14,detach_gpr+104(%rip)
mov %r15,detach_gpr+112(%rip)
movdqu %xmm0,detach_vector_got+0(%rip)
movdqu %xmm1,detach_vector_got+16(%rip)
movdqu %xmm2,detach_vector_got+32(%rip)
movdqu %xmm3,detach_vector_got+48(%rip)
movdqu %xmm4,detach_vector_got+64(%rip)
movdqu %xmm5,detach_vector_got+80(%rip)
movdqu %xmm6,detach_vector_got+96(%rip)
movdqu %xmm7,detach_vector_got+112(%rip)
movdqu %xmm8,detach_vector_got+128(%rip)
movdqu %xmm9,detach_vector_got+144(%rip)
movdqu %xmm10,detach_vector_got+160(%rip)
movdqu %xmm11,detach_vector_got+176(%rip)
movdqu %xmm12,detach_vector_got+192(%rip)
movdqu %xmm13,detach_vector_got+208(%rip)
movdqu %xmm14,detach_vector_got+224(%rip)
movdqu %xmm15,detach_vector_got+240(%rip)
mov detach_saved_stack(%rip),%rsp
cld
pop %r15
pop %r14
pop %r13
pop %r12
pop %rbx
pop %rbp
ret
.globl detach_gprs_cache_end
detach_gprs_cache_end:
.size detach_gprs_cache,.-detach_gprs_cache
.p2align 16
.globl detach_gprs_selfmod1
.type detach_gprs_selfmod1,@function
detach_gprs_selfmod1:
push %rbp
push %rbx
push %r12
push %r13
push %r14
push %r15
mov %rsp,detach_saved_stack(%rip)
movdqu detach_vector_expect+0(%rip),%xmm0
movdqu detach_vector_expect+16(%rip),%xmm1
movdqu detach_vector_expect+32(%rip),%xmm2
movdqu detach_vector_expect+48(%rip),%xmm3
movdqu detach_vector_expect+64(%rip),%xmm4
movdqu detach_vector_expect+80(%rip),%xmm5
movdqu detach_vector_expect+96(%rip),%xmm6
movdqu detach_vector_expect+112(%rip),%xmm7
movdqu detach_vector_expect+128(%rip),%xmm8
movdqu detach_vector_expect+144(%rip),%xmm9
movdqu detach_vector_expect+160(%rip),%xmm10
movdqu detach_vector_expect+176(%rip),%xmm11
movdqu detach_vector_expect+192(%rip),%xmm12
movdqu detach_vector_expect+208(%rip),%xmm13
movdqu detach_vector_expect+224(%rip),%xmm14
movdqu detach_vector_expect+240(%rip),%xmm15
movabs $0x12345678abcdef01,%rax
movabs $0x2345678abcdef012,%rcx
movabs $0x345678abcdef0123,%rdx
movabs $0x45678abcdef01234,%rbx
movabs $0x5678abcdef012345,%rbp
movabs $0x678abcdef0123456,%rsi
movabs $0x78abcdef01234567,%rdi
movabs $0x1234567890abcdef,%r8
movabs $0xf1234567890abcde,%r9
movabs $0xef1234567890abcd,%r10
movabs $0xdef1234567890abc,%r11
movabs $0xcdef1234567890ab,%r12
movabs $0xbcdef1234567890a,%r13
movabs $0xabcdef1234567890,%r14
movabs $0xabcdef123456789,%r15
mov $0,%eax
movl $1,detach_ready(%rip)
.Lgprs_selfmod1_loop:
lea 1(%rax),%eax
lea .Lgprs_selfmod1_immediate-4(%rip),%rdx
mov %eax,(%rdx)
mov $0,%rdx
.Lgprs_selfmod1_immediate:
movl $1,detach_progress(%rip)
cmpb $1,detach_released(%rip)
jne .Lgprs_selfmod1_loop
mov %rax,detach_gpr+0(%rip)
mov %rcx,detach_gpr+8(%rip)
mov %rdx,detach_gpr+16(%rip)
mov %rbx,detach_gpr+24(%rip)
mov %rbp,detach_gpr+32(%rip)
mov %rsi,detach_gpr+40(%rip)
mov %rdi,detach_gpr+48(%rip)
mov %r8,detach_gpr+56(%rip)
mov %r9,detach_gpr+64(%rip)
mov %r10,detach_gpr+72(%rip)
mov %r11,detach_gpr+80(%rip)
mov %r12,detach_gpr+88(%rip)
mov %r13,detach_gpr+96(%rip)
mov %r14,detach_gpr+104(%rip)
mov %r15,detach_gpr+112(%rip)
movdqu %xmm0,detach_vector_got+0(%rip)
movdqu %xmm1,detach_vector_got+16(%rip)
movdqu %xmm2,detach_vector_got+32(%rip)
movdqu %xmm3,detach_vector_got+48(%rip)
movdqu %xmm4,detach_vector_got+64(%rip)
movdqu %xmm5,detach_vector_got+80(%rip)
movdqu %xmm6,detach_vector_got+96(%rip)
movdqu %xmm7,detach_vector_got+112(%rip)
movdqu %xmm8,detach_vector_got+128(%rip)
movdqu %xmm9,detach_vector_got+144(%rip)
movdqu %xmm10,detach_vector_got+160(%rip)
movdqu %xmm11,detach_vector_got+176(%rip)
movdqu %xmm12,detach_vector_got+192(%rip)
movdqu %xmm13,detach_vector_got+208(%rip)
movdqu %xmm14,detach_vector_got+224(%rip)
movdqu %xmm15,detach_vector_got+240(%rip)
mov detach_saved_stack(%rip),%rsp
cld
pop %r15
pop %r14
pop %r13
pop %r12
pop %rbx
pop %rbp
ret
.globl detach_gprs_selfmod1_end
detach_gprs_selfmod1_end:
.size detach_gprs_selfmod1,.-detach_gprs_selfmod1
.p2align 16
.globl detach_gprs_selfmod2
.type detach_gprs_selfmod2,@function
detach_gprs_selfmod2:
push %rbp
push %rbx
push %r12
push %r13
push %r14
push %r15
mov %rsp,detach_saved_stack(%rip)
movdqu detach_vector_expect+0(%rip),%xmm0
movdqu detach_vector_expect+16(%rip),%xmm1
movdqu detach_vector_expect+32(%rip),%xmm2
movdqu detach_vector_expect+48(%rip),%xmm3
movdqu detach_vector_expect+64(%rip),%xmm4
movdqu detach_vector_expect+80(%rip),%xmm5
movdqu detach_vector_expect+96(%rip),%xmm6
movdqu detach_vector_expect+112(%rip),%xmm7
movdqu detach_vector_expect+128(%rip),%xmm8
movdqu detach_vector_expect+144(%rip),%xmm9
movdqu detach_vector_expect+160(%rip),%xmm10
movdqu detach_vector_expect+176(%rip),%xmm11
movdqu detach_vector_expect+192(%rip),%xmm12
movdqu detach_vector_expect+208(%rip),%xmm13
movdqu detach_vector_expect+224(%rip),%xmm14
movdqu detach_vector_expect+240(%rip),%xmm15
movabs $0x12345678abcdef01,%rax
movabs $0x2345678abcdef012,%rcx
movabs $0x345678abcdef0123,%rdx
movabs $0x45678abcdef01234,%rbx
movabs $0x5678abcdef012345,%rbp
movabs $0x678abcdef0123456,%rsi
movabs $0x78abcdef01234567,%rdi
movabs $0x1234567890abcdef,%r8
movabs $0xf1234567890abcde,%r9
movabs $0xef1234567890abcd,%r10
movabs $0xdef1234567890abc,%r11
movabs $0xcdef1234567890ab,%r12
movabs $0xbcdef1234567890a,%r13
movabs $0xabcdef1234567890,%r14
movabs $0xabcdef123456789,%r15
mov $0,%ebx
movl $1,detach_ready(%rip)
.Lgprs_selfmod2_loop:
lea 1(%rbx),%ebx
lea .Lgprs_selfmod2_immediate-4(%rip),%rcx
mov %ebx,(%rcx)
mov $0,%rcx
.Lgprs_selfmod2_immediate:
movl $1,detach_progress(%rip)
cmpb $1,detach_released(%rip)
jne .Lgprs_selfmod2_loop
mov %rax,detach_gpr+0(%rip)
mov %rcx,detach_gpr+8(%rip)
mov %rdx,detach_gpr+16(%rip)
mov %rbx,detach_gpr+24(%rip)
mov %rbp,detach_gpr+32(%rip)
mov %rsi,detach_gpr+40(%rip)
mov %rdi,detach_gpr+48(%rip)
mov %r8,detach_gpr+56(%rip)
mov %r9,detach_gpr+64(%rip)
mov %r10,detach_gpr+72(%rip)
mov %r11,detach_gpr+80(%rip)
mov %r12,detach_gpr+88(%rip)
mov %r13,detach_gpr+96(%rip)
mov %r14,detach_gpr+104(%rip)
mov %r15,detach_gpr+112(%rip)
movdqu %xmm0,detach_vector_got+0(%rip)
movdqu %xmm1,detach_vector_got+16(%rip)
movdqu %xmm2,detach_vector_got+32(%rip)
movdqu %xmm3,detach_vector_got+48(%rip)
movdqu %xmm4,detach_vector_got+64(%rip)
movdqu %xmm5,detach_vector_got+80(%rip)
movdqu %xmm6,detach_vector_got+96(%rip)
movdqu %xmm7,detach_vector_got+112(%rip)
movdqu %xmm8,detach_vector_got+128(%rip)
movdqu %xmm9,detach_vector_got+144(%rip)
movdqu %xmm10,detach_vector_got+160(%rip)
movdqu %xmm11,detach_vector_got+176(%rip)
movdqu %xmm12,detach_vector_got+192(%rip)
movdqu %xmm13,detach_vector_got+208(%rip)
movdqu %xmm14,detach_vector_got+224(%rip)
movdqu %xmm15,detach_vector_got+240(%rip)
mov detach_saved_stack(%rip),%rsp
cld
pop %r15
pop %r14
pop %r13
pop %r12
pop %rbx
pop %rbp
ret
.globl detach_gprs_selfmod2_end
detach_gprs_selfmod2_end:
.size detach_gprs_selfmod2,.-detach_gprs_selfmod2
.p2align 16
.globl detach_flags_cache
.type detach_flags_cache,@function
detach_flags_cache:
push %rbp
push %rbx
push %r12
push %r13
push %r14
push %r15
mov %rsp,detach_saved_stack(%rip)
mov $0,%ecx
pushq $0xed7
popfq
movl $1,detach_ready(%rip)
.Lflags_cache_loop:
movl $1,detach_progress(%rip)
movzbl detach_released(%rip),%ecx
jrcxz .Lflags_cache_loop
pushfq
pop %rax
mov %rax,detach_status(%rip)
cld
mov detach_saved_stack(%rip),%rsp
cld
pop %r15
pop %r14
pop %r13
pop %r12
pop %rbx
pop %rbp
ret
.globl detach_flags_cache_end
detach_flags_cache_end:
.size detach_flags_cache,.-detach_flags_cache
.p2align 16
.globl detach_flags_selfmod
.type detach_flags_selfmod,@function
detach_flags_selfmod:
push %rbp
push %rbx
push %r12
push %r13
push %r14
push %r15
mov %rsp,detach_saved_stack(%rip)
mov $0,%eax
mov $0,%ecx
pushq $0xed7
popfq
movl $1,detach_ready(%rip)
.Lflags_selfmod_loop:
lea 1(%rax),%eax
lea .Lflags_selfmod_immediate-4(%rip),%rdx
mov %eax,(%rdx)
mov $0,%rdx
.Lflags_selfmod_immediate:
movl $1,detach_progress(%rip)
movzbl detach_released(%rip),%ecx
jrcxz .Lflags_selfmod_loop
pushfq
pop %rax
mov %rax,detach_status(%rip)
cld
mov detach_saved_stack(%rip),%rsp
cld
pop %r15
pop %r14
pop %r13
pop %r12
pop %rbx
pop %rbp
ret
.globl detach_flags_selfmod_end
detach_flags_selfmod_end:
.size detach_flags_selfmod,.-detach_flags_selfmod
.p2align 16
.globl detach_sp_cache
.type detach_sp_cache,@function
detach_sp_cache:
push %rbp
push %rbx
push %r12
push %r13
push %r14
push %r15
mov %rsp,detach_saved_stack(%rip)
mov detach_safe_stack(%rip),%rsp
movabs $0x12345678abcdef01,%rax
mov %rax,-8(%rsp)
movabs $0x345678abcdef0123,%rdx
mov %rdx,-16(%rsp)
movl $1,detach_ready(%rip)
.Lsp_cache_loop:
movl $1,detach_progress(%rip)
cmpb $1,detach_released(%rip)
jne .Lsp_cache_loop
mov %rsp,detach_stack_observed(%rip)
mov -8(%rsp),%rax
mov %rax,detach_redzone(%rip)
mov -16(%rsp),%rax
mov %rax,detach_redzone+8(%rip)
mov detach_saved_stack(%rip),%rsp
cld
pop %r15
pop %r14
pop %r13
pop %r12
pop %rbx
pop %rbp
ret
.globl detach_sp_cache_end
detach_sp_cache_end:
.size detach_sp_cache,.-detach_sp_cache
.p2align 16
.globl detach_sp_selfmod
.type detach_sp_selfmod,@function
detach_sp_selfmod:
push %rbp
push %rbx
push %r12
push %r13
push %r14
push %r15
mov %rsp,detach_saved_stack(%rip)
mov detach_safe_stack(%rip),%rsp
movabs $0x12345678abcdef01,%rax
mov %rax,-8(%rsp)
movabs $0x345678abcdef0123,%rdx
mov %rdx,-16(%rsp)
mov $0,%eax
movl $1,detach_ready(%rip)
.Lsp_selfmod_loop:
lea 1(%rax),%eax
lea .Lsp_selfmod_immediate-4(%rip),%rdx
mov %eax,(%rdx)
mov $0,%rdx
.Lsp_selfmod_immediate:
movl $1,detach_progress(%rip)
cmpb $1,detach_released(%rip)
jne .Lsp_selfmod_loop
mov %rsp,detach_stack_observed(%rip)
mov -8(%rsp),%rax
mov %rax,detach_redzone(%rip)
mov -16(%rsp),%rax
mov %rax,detach_redzone+8(%rip)
mov detach_saved_stack(%rip),%rsp
cld
pop %r15
pop %r14
pop %r13
pop %r12
pop %rbx
pop %rbp
ret
.globl detach_sp_selfmod_end
detach_sp_selfmod_end:
.size detach_sp_selfmod,.-detach_sp_selfmod
.p2align 16
#elif defined(__aarch64__)
.text
.p2align 16
.globl detach_gprs_cache
.type detach_gprs_cache,%function
detach_gprs_cache:
stp x19,x20,[sp,#-16]!
stp x21,x22,[sp,#-16]!
stp x23,x24,[sp,#-16]!
stp x25,x26,[sp,#-16]!
stp x27,x28,[sp,#-16]!
stp x29,x30,[sp,#-16]!
stp q8,q9,[sp,#-32]!
stp q10,q11,[sp,#-32]!
stp q12,q13,[sp,#-32]!
stp q14,q15,[sp,#-32]!
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
mov x1,sp
str x1,[x0]
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
adrp x0,detach_vector_expect
add x0,x0,:lo12:detach_vector_expect
ldr q0,[x0,#0]
ldr q1,[x0,#16]
ldr q2,[x0,#32]
ldr q3,[x0,#48]
ldr q4,[x0,#64]
ldr q5,[x0,#80]
ldr q6,[x0,#96]
ldr q7,[x0,#112]
ldr q8,[x0,#128]
ldr q9,[x0,#144]
ldr q10,[x0,#160]
ldr q11,[x0,#176]
ldr q12,[x0,#192]
ldr q13,[x0,#208]
ldr q14,[x0,#224]
ldr q15,[x0,#240]
ldr q16,[x0,#256]
ldr q17,[x0,#272]
ldr q18,[x0,#288]
ldr q19,[x0,#304]
ldr q20,[x0,#320]
ldr q21,[x0,#336]
ldr q22,[x0,#352]
ldr q23,[x0,#368]
ldr q24,[x0,#384]
ldr q25,[x0,#400]
ldr q26,[x0,#416]
ldr q27,[x0,#432]
ldr q28,[x0,#448]
ldr q29,[x0,#464]
ldr q30,[x0,#480]
ldr q31,[x0,#496]
movz x0,#52719
movk x0,#35243,lsl #16
movk x0,#17767,lsl #32
movk x0,#291,lsl #48
movz x1,#57072
movk x1,#39612,lsl #16
movk x1,#22136,lsl #32
movk x1,#4660,lsl #48
movz x2,#61185
movk x2,#43981,lsl #16
movk x2,#26505,lsl #32
movk x2,#9029,lsl #48
movz x3,#61458
movk x3,#48350,lsl #16
movk x3,#30874,lsl #32
movk x3,#13398,lsl #48
movz x4,#291
movk x4,#52719,lsl #16
movk x4,#35243,lsl #32
movk x4,#17767,lsl #48
movz x5,#4677
movk x5,#57072,lsl #16
movk x5,#39612,lsl #32
movk x5,#22136,lsl #48
movz x6,#9029
movk x6,#61185,lsl #16
movk x6,#43981,lsl #32
movk x6,#26505,lsl #48
movz x7,#13398
movk x7,#61458,lsl #16
movk x7,#48350,lsl #32
movk x7,#30874,lsl #48
movz x8,#17767
movk x8,#291,lsl #16
movk x8,#52719,lsl #32
movk x8,#35243,lsl #48
movz x9,#22136
movk x9,#4660,lsl #16
movk x9,#57072,lsl #32
movk x9,#39612,lsl #48
movz x10,#26505
movk x10,#9029,lsl #16
movk x10,#61185,lsl #32
movk x10,#43981,lsl #48
movz x11,#30874
movk x11,#13398,lsl #16
movk x11,#61458,lsl #32
movk x11,#48350,lsl #48
movz x12,#35243
movk x12,#17767,lsl #16
movk x12,#291,lsl #32
movk x12,#52719,lsl #48
movz x13,#39612
movk x13,#22136,lsl #16
movk x13,#4660,lsl #32
movk x13,#57072,lsl #48
movz x14,#43981
movk x14,#26505,lsl #16
movk x14,#9029,lsl #32
movk x14,#61185,lsl #48
movz x15,#48350
movk x15,#30874,lsl #16
movk x15,#13398,lsl #32
movk x15,#61458,lsl #48
movz x16,#52719
movk x16,#35243,lsl #16
movk x16,#17767,lsl #32
movk x16,#61731,lsl #48
movz x17,#57072
movk x17,#39612,lsl #16
movk x17,#22136,lsl #32
movk x17,#62004,lsl #48
movz x18,#61185
movk x18,#43981,lsl #16
movk x18,#26505,lsl #32
movk x18,#62277,lsl #48
movz x19,#61458
movk x19,#48350,lsl #16
movk x19,#30874,lsl #32
movk x19,#62550,lsl #48
movz x20,#291
movk x20,#52719,lsl #16
movk x20,#35243,lsl #32
movk x20,#62823,lsl #48
movz x21,#4677
movk x21,#57072,lsl #16
movk x21,#39612,lsl #32
movk x21,#63096,lsl #48
movz x22,#9029
movk x22,#61185,lsl #16
movk x22,#43981,lsl #32
movk x22,#63369,lsl #48
movz x23,#13398
movk x23,#61458,lsl #16
movk x23,#48350,lsl #32
movk x23,#63642,lsl #48
movz x24,#17767
movk x24,#291,lsl #16
movk x24,#52719,lsl #32
movk x24,#63915,lsl #48
movz x25,#22136
movk x25,#4660,lsl #16
movk x25,#57072,lsl #32
movk x25,#64188,lsl #48
movz x26,#26505
movk x26,#9029,lsl #16
movk x26,#61185,lsl #32
movk x26,#64461,lsl #48
movz x27,#30874
movk x27,#13398,lsl #16
movk x27,#61458,lsl #32
movk x27,#64734,lsl #48
movz x28,#35243
movk x28,#17767,lsl #16
movk x28,#291,lsl #32
movk x28,#65007,lsl #48
movz x29,#39612
movk x29,#22136,lsl #16
movk x29,#4660,lsl #32
movk x29,#65264,lsl #48
movz x30,#43981
movk x30,#26505,lsl #16
movk x30,#9029,lsl #32
movk x30,#65281,lsl #48
adrp x0,detach_ready
add x0,x0,:lo12:detach_ready
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
.Lgprs_cache_loop:
adrp x0,detach_progress
add x0,x0,:lo12:detach_progress
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
adrp x0,detach_released
add x0,x0,:lo12:detach_released
ldrb w0,[x0]
cmp w0,#1
b.ne .Lgprs_cache_loop
adrp x0,detach_gpr
add x0,x0,:lo12:detach_gpr
str x1,[x0,#8]
str x2,[x0,#16]
str x3,[x0,#24]
str x4,[x0,#32]
str x5,[x0,#40]
str x6,[x0,#48]
str x7,[x0,#56]
str x8,[x0,#64]
str x9,[x0,#72]
str x10,[x0,#80]
str x11,[x0,#88]
str x12,[x0,#96]
str x13,[x0,#104]
str x14,[x0,#112]
str x15,[x0,#120]
str x16,[x0,#128]
str x17,[x0,#136]
str x18,[x0,#144]
str x19,[x0,#152]
str x20,[x0,#160]
str x21,[x0,#168]
str x22,[x0,#176]
str x23,[x0,#184]
str x24,[x0,#192]
str x25,[x0,#200]
str x26,[x0,#208]
str x27,[x0,#216]
str x28,[x0,#224]
str x29,[x0,#232]
str x30,[x0,#240]
adrp x0,detach_vector_got
add x0,x0,:lo12:detach_vector_got
str q0,[x0,#0]
str q1,[x0,#16]
str q2,[x0,#32]
str q3,[x0,#48]
str q4,[x0,#64]
str q5,[x0,#80]
str q6,[x0,#96]
str q7,[x0,#112]
str q8,[x0,#128]
str q9,[x0,#144]
str q10,[x0,#160]
str q11,[x0,#176]
str q12,[x0,#192]
str q13,[x0,#208]
str q14,[x0,#224]
str q15,[x0,#240]
str q16,[x0,#256]
str q17,[x0,#272]
str q18,[x0,#288]
str q19,[x0,#304]
str q20,[x0,#320]
str q21,[x0,#336]
str q22,[x0,#352]
str q23,[x0,#368]
str q24,[x0,#384]
str q25,[x0,#400]
str q26,[x0,#416]
str q27,[x0,#432]
str q28,[x0,#448]
str q29,[x0,#464]
str q30,[x0,#480]
str q31,[x0,#496]
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
ldr x0,[x0]
mov sp,x0
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
ldr x1,[x0]
msr nzcv,x1
ldr x1,[x0,#8]
msr fpcr,x1
ldr x1,[x0,#16]
msr fpsr,x1
ldp q14,q15,[sp],#32
ldp q12,q13,[sp],#32
ldp q10,q11,[sp],#32
ldp q8,q9,[sp],#32
ldp x29,x30,[sp],#16
ldp x27,x28,[sp],#16
ldp x25,x26,[sp],#16
ldp x23,x24,[sp],#16
ldp x21,x22,[sp],#16
ldp x19,x20,[sp],#16
ret
.globl detach_gprs_cache_end
detach_gprs_cache_end:
.size detach_gprs_cache,.-detach_gprs_cache
.p2align 16
.globl detach_gprs_selfmod1
.type detach_gprs_selfmod1,%function
detach_gprs_selfmod1:
stp x19,x20,[sp,#-16]!
stp x21,x22,[sp,#-16]!
stp x23,x24,[sp,#-16]!
stp x25,x26,[sp,#-16]!
stp x27,x28,[sp,#-16]!
stp x29,x30,[sp,#-16]!
stp q8,q9,[sp,#-32]!
stp q10,q11,[sp,#-32]!
stp q12,q13,[sp,#-32]!
stp q14,q15,[sp,#-32]!
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
mov x1,sp
str x1,[x0]
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
adrp x0,detach_vector_expect
add x0,x0,:lo12:detach_vector_expect
ldr q0,[x0,#0]
ldr q1,[x0,#16]
ldr q2,[x0,#32]
ldr q3,[x0,#48]
ldr q4,[x0,#64]
ldr q5,[x0,#80]
ldr q6,[x0,#96]
ldr q7,[x0,#112]
ldr q8,[x0,#128]
ldr q9,[x0,#144]
ldr q10,[x0,#160]
ldr q11,[x0,#176]
ldr q12,[x0,#192]
ldr q13,[x0,#208]
ldr q14,[x0,#224]
ldr q15,[x0,#240]
ldr q16,[x0,#256]
ldr q17,[x0,#272]
ldr q18,[x0,#288]
ldr q19,[x0,#304]
ldr q20,[x0,#320]
ldr q21,[x0,#336]
ldr q22,[x0,#352]
ldr q23,[x0,#368]
ldr q24,[x0,#384]
ldr q25,[x0,#400]
ldr q26,[x0,#416]
ldr q27,[x0,#432]
ldr q28,[x0,#448]
ldr q29,[x0,#464]
ldr q30,[x0,#480]
ldr q31,[x0,#496]
movz x0,#52719
movk x0,#35243,lsl #16
movk x0,#17767,lsl #32
movk x0,#291,lsl #48
movz x1,#57072
movk x1,#39612,lsl #16
movk x1,#22136,lsl #32
movk x1,#4660,lsl #48
movz x2,#61185
movk x2,#43981,lsl #16
movk x2,#26505,lsl #32
movk x2,#9029,lsl #48
movz x3,#61458
movk x3,#48350,lsl #16
movk x3,#30874,lsl #32
movk x3,#13398,lsl #48
movz x4,#291
movk x4,#52719,lsl #16
movk x4,#35243,lsl #32
movk x4,#17767,lsl #48
movz x5,#4677
movk x5,#57072,lsl #16
movk x5,#39612,lsl #32
movk x5,#22136,lsl #48
movz x6,#9029
movk x6,#61185,lsl #16
movk x6,#43981,lsl #32
movk x6,#26505,lsl #48
movz x7,#13398
movk x7,#61458,lsl #16
movk x7,#48350,lsl #32
movk x7,#30874,lsl #48
movz x8,#17767
movk x8,#291,lsl #16
movk x8,#52719,lsl #32
movk x8,#35243,lsl #48
movz x9,#22136
movk x9,#4660,lsl #16
movk x9,#57072,lsl #32
movk x9,#39612,lsl #48
movz x10,#26505
movk x10,#9029,lsl #16
movk x10,#61185,lsl #32
movk x10,#43981,lsl #48
movz x11,#30874
movk x11,#13398,lsl #16
movk x11,#61458,lsl #32
movk x11,#48350,lsl #48
movz x12,#35243
movk x12,#17767,lsl #16
movk x12,#291,lsl #32
movk x12,#52719,lsl #48
movz x13,#39612
movk x13,#22136,lsl #16
movk x13,#4660,lsl #32
movk x13,#57072,lsl #48
movz x14,#43981
movk x14,#26505,lsl #16
movk x14,#9029,lsl #32
movk x14,#61185,lsl #48
movz x15,#48350
movk x15,#30874,lsl #16
movk x15,#13398,lsl #32
movk x15,#61458,lsl #48
movz x16,#52719
movk x16,#35243,lsl #16
movk x16,#17767,lsl #32
movk x16,#61731,lsl #48
movz x17,#57072
movk x17,#39612,lsl #16
movk x17,#22136,lsl #32
movk x17,#62004,lsl #48
movz x18,#61185
movk x18,#43981,lsl #16
movk x18,#26505,lsl #32
movk x18,#62277,lsl #48
movz x19,#61458
movk x19,#48350,lsl #16
movk x19,#30874,lsl #32
movk x19,#62550,lsl #48
movz x20,#291
movk x20,#52719,lsl #16
movk x20,#35243,lsl #32
movk x20,#62823,lsl #48
movz x21,#4677
movk x21,#57072,lsl #16
movk x21,#39612,lsl #32
movk x21,#63096,lsl #48
movz x22,#9029
movk x22,#61185,lsl #16
movk x22,#43981,lsl #32
movk x22,#63369,lsl #48
movz x23,#13398
movk x23,#61458,lsl #16
movk x23,#48350,lsl #32
movk x23,#63642,lsl #48
movz x24,#17767
movk x24,#291,lsl #16
movk x24,#52719,lsl #32
movk x24,#63915,lsl #48
movz x25,#22136
movk x25,#4660,lsl #16
movk x25,#57072,lsl #32
movk x25,#64188,lsl #48
movz x26,#26505
movk x26,#9029,lsl #16
movk x26,#61185,lsl #32
movk x26,#64461,lsl #48
movz x27,#30874
movk x27,#13398,lsl #16
movk x27,#61458,lsl #32
movk x27,#64734,lsl #48
movz x28,#35243
movk x28,#17767,lsl #16
movk x28,#291,lsl #32
movk x28,#65007,lsl #48
movz x29,#39612
movk x29,#22136,lsl #16
movk x29,#4660,lsl #32
movk x29,#65264,lsl #48
movz x30,#43981
movk x30,#26505,lsl #16
movk x30,#9029,lsl #32
movk x30,#65281,lsl #48
movz w2,#0
movk w2,#0x5280,lsl #16
adrp x0,detach_ready
add x0,x0,:lo12:detach_ready
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
.Lgprs_selfmod1_loop:
ubfx w0,w2,#5,#16
add w0,w0,#1
bfi w2,w0,#5,#16
adr x0,.Lgprs_selfmod1_patch
str w2,[x0]
dc cvau,x0
dsb ish
ic ivau,x0
dsb ish
isb
.Lgprs_selfmod1_patch:
movz w0,#0
adrp x0,detach_progress
add x0,x0,:lo12:detach_progress
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
adrp x0,detach_released
add x0,x0,:lo12:detach_released
ldrb w0,[x0]
cmp w0,#1
b.ne .Lgprs_selfmod1_loop
adrp x0,detach_gpr
add x0,x0,:lo12:detach_gpr
str x1,[x0,#8]
str x2,[x0,#16]
str x3,[x0,#24]
str x4,[x0,#32]
str x5,[x0,#40]
str x6,[x0,#48]
str x7,[x0,#56]
str x8,[x0,#64]
str x9,[x0,#72]
str x10,[x0,#80]
str x11,[x0,#88]
str x12,[x0,#96]
str x13,[x0,#104]
str x14,[x0,#112]
str x15,[x0,#120]
str x16,[x0,#128]
str x17,[x0,#136]
str x18,[x0,#144]
str x19,[x0,#152]
str x20,[x0,#160]
str x21,[x0,#168]
str x22,[x0,#176]
str x23,[x0,#184]
str x24,[x0,#192]
str x25,[x0,#200]
str x26,[x0,#208]
str x27,[x0,#216]
str x28,[x0,#224]
str x29,[x0,#232]
str x30,[x0,#240]
adrp x0,detach_vector_got
add x0,x0,:lo12:detach_vector_got
str q0,[x0,#0]
str q1,[x0,#16]
str q2,[x0,#32]
str q3,[x0,#48]
str q4,[x0,#64]
str q5,[x0,#80]
str q6,[x0,#96]
str q7,[x0,#112]
str q8,[x0,#128]
str q9,[x0,#144]
str q10,[x0,#160]
str q11,[x0,#176]
str q12,[x0,#192]
str q13,[x0,#208]
str q14,[x0,#224]
str q15,[x0,#240]
str q16,[x0,#256]
str q17,[x0,#272]
str q18,[x0,#288]
str q19,[x0,#304]
str q20,[x0,#320]
str q21,[x0,#336]
str q22,[x0,#352]
str q23,[x0,#368]
str q24,[x0,#384]
str q25,[x0,#400]
str q26,[x0,#416]
str q27,[x0,#432]
str q28,[x0,#448]
str q29,[x0,#464]
str q30,[x0,#480]
str q31,[x0,#496]
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
ldr x0,[x0]
mov sp,x0
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
ldr x1,[x0]
msr nzcv,x1
ldr x1,[x0,#8]
msr fpcr,x1
ldr x1,[x0,#16]
msr fpsr,x1
ldp q14,q15,[sp],#32
ldp q12,q13,[sp],#32
ldp q10,q11,[sp],#32
ldp q8,q9,[sp],#32
ldp x29,x30,[sp],#16
ldp x27,x28,[sp],#16
ldp x25,x26,[sp],#16
ldp x23,x24,[sp],#16
ldp x21,x22,[sp],#16
ldp x19,x20,[sp],#16
ret
.globl detach_gprs_selfmod1_end
detach_gprs_selfmod1_end:
.size detach_gprs_selfmod1,.-detach_gprs_selfmod1
.p2align 16
.globl detach_gprs_selfmod2
.type detach_gprs_selfmod2,%function
detach_gprs_selfmod2:
stp x19,x20,[sp,#-16]!
stp x21,x22,[sp,#-16]!
stp x23,x24,[sp,#-16]!
stp x25,x26,[sp,#-16]!
stp x27,x28,[sp,#-16]!
stp x29,x30,[sp,#-16]!
stp q8,q9,[sp,#-32]!
stp q10,q11,[sp,#-32]!
stp q12,q13,[sp,#-32]!
stp q14,q15,[sp,#-32]!
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
mov x1,sp
str x1,[x0]
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
adrp x0,detach_vector_expect
add x0,x0,:lo12:detach_vector_expect
ldr q0,[x0,#0]
ldr q1,[x0,#16]
ldr q2,[x0,#32]
ldr q3,[x0,#48]
ldr q4,[x0,#64]
ldr q5,[x0,#80]
ldr q6,[x0,#96]
ldr q7,[x0,#112]
ldr q8,[x0,#128]
ldr q9,[x0,#144]
ldr q10,[x0,#160]
ldr q11,[x0,#176]
ldr q12,[x0,#192]
ldr q13,[x0,#208]
ldr q14,[x0,#224]
ldr q15,[x0,#240]
ldr q16,[x0,#256]
ldr q17,[x0,#272]
ldr q18,[x0,#288]
ldr q19,[x0,#304]
ldr q20,[x0,#320]
ldr q21,[x0,#336]
ldr q22,[x0,#352]
ldr q23,[x0,#368]
ldr q24,[x0,#384]
ldr q25,[x0,#400]
ldr q26,[x0,#416]
ldr q27,[x0,#432]
ldr q28,[x0,#448]
ldr q29,[x0,#464]
ldr q30,[x0,#480]
ldr q31,[x0,#496]
movz x0,#52719
movk x0,#35243,lsl #16
movk x0,#17767,lsl #32
movk x0,#291,lsl #48
movz x1,#57072
movk x1,#39612,lsl #16
movk x1,#22136,lsl #32
movk x1,#4660,lsl #48
movz x2,#61185
movk x2,#43981,lsl #16
movk x2,#26505,lsl #32
movk x2,#9029,lsl #48
movz x3,#61458
movk x3,#48350,lsl #16
movk x3,#30874,lsl #32
movk x3,#13398,lsl #48
movz x4,#291
movk x4,#52719,lsl #16
movk x4,#35243,lsl #32
movk x4,#17767,lsl #48
movz x5,#4677
movk x5,#57072,lsl #16
movk x5,#39612,lsl #32
movk x5,#22136,lsl #48
movz x6,#9029
movk x6,#61185,lsl #16
movk x6,#43981,lsl #32
movk x6,#26505,lsl #48
movz x7,#13398
movk x7,#61458,lsl #16
movk x7,#48350,lsl #32
movk x7,#30874,lsl #48
movz x8,#17767
movk x8,#291,lsl #16
movk x8,#52719,lsl #32
movk x8,#35243,lsl #48
movz x9,#22136
movk x9,#4660,lsl #16
movk x9,#57072,lsl #32
movk x9,#39612,lsl #48
movz x10,#26505
movk x10,#9029,lsl #16
movk x10,#61185,lsl #32
movk x10,#43981,lsl #48
movz x11,#30874
movk x11,#13398,lsl #16
movk x11,#61458,lsl #32
movk x11,#48350,lsl #48
movz x12,#35243
movk x12,#17767,lsl #16
movk x12,#291,lsl #32
movk x12,#52719,lsl #48
movz x13,#39612
movk x13,#22136,lsl #16
movk x13,#4660,lsl #32
movk x13,#57072,lsl #48
movz x14,#43981
movk x14,#26505,lsl #16
movk x14,#9029,lsl #32
movk x14,#61185,lsl #48
movz x15,#48350
movk x15,#30874,lsl #16
movk x15,#13398,lsl #32
movk x15,#61458,lsl #48
movz x16,#52719
movk x16,#35243,lsl #16
movk x16,#17767,lsl #32
movk x16,#61731,lsl #48
movz x17,#57072
movk x17,#39612,lsl #16
movk x17,#22136,lsl #32
movk x17,#62004,lsl #48
movz x18,#61185
movk x18,#43981,lsl #16
movk x18,#26505,lsl #32
movk x18,#62277,lsl #48
movz x19,#61458
movk x19,#48350,lsl #16
movk x19,#30874,lsl #32
movk x19,#62550,lsl #48
movz x20,#291
movk x20,#52719,lsl #16
movk x20,#35243,lsl #32
movk x20,#62823,lsl #48
movz x21,#4677
movk x21,#57072,lsl #16
movk x21,#39612,lsl #32
movk x21,#63096,lsl #48
movz x22,#9029
movk x22,#61185,lsl #16
movk x22,#43981,lsl #32
movk x22,#63369,lsl #48
movz x23,#13398
movk x23,#61458,lsl #16
movk x23,#48350,lsl #32
movk x23,#63642,lsl #48
movz x24,#17767
movk x24,#291,lsl #16
movk x24,#52719,lsl #32
movk x24,#63915,lsl #48
movz x25,#22136
movk x25,#4660,lsl #16
movk x25,#57072,lsl #32
movk x25,#64188,lsl #48
movz x26,#26505
movk x26,#9029,lsl #16
movk x26,#61185,lsl #32
movk x26,#64461,lsl #48
movz x27,#30874
movk x27,#13398,lsl #16
movk x27,#61458,lsl #32
movk x27,#64734,lsl #48
movz x28,#35243
movk x28,#17767,lsl #16
movk x28,#291,lsl #32
movk x28,#65007,lsl #48
movz x29,#39612
movk x29,#22136,lsl #16
movk x29,#4660,lsl #32
movk x29,#65264,lsl #48
movz x30,#43981
movk x30,#26505,lsl #16
movk x30,#9029,lsl #32
movk x30,#65281,lsl #48
movz w5,#6
movk w5,#0x5280,lsl #16
adrp x0,detach_ready
add x0,x0,:lo12:detach_ready
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
.Lgprs_selfmod2_loop:
ubfx w6,w5,#5,#16
add w6,w6,#1
bfi w5,w6,#5,#16
adr x6,.Lgprs_selfmod2_patch
str w5,[x6]
dc cvau,x6
dsb ish
ic ivau,x6
dsb ish
isb
.Lgprs_selfmod2_patch:
movz w6,#0
adrp x0,detach_progress
add x0,x0,:lo12:detach_progress
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
adrp x0,detach_released
add x0,x0,:lo12:detach_released
ldrb w0,[x0]
cmp w0,#1
b.ne .Lgprs_selfmod2_loop
adrp x0,detach_gpr
add x0,x0,:lo12:detach_gpr
str x1,[x0,#8]
str x2,[x0,#16]
str x3,[x0,#24]
str x4,[x0,#32]
str x5,[x0,#40]
str x6,[x0,#48]
str x7,[x0,#56]
str x8,[x0,#64]
str x9,[x0,#72]
str x10,[x0,#80]
str x11,[x0,#88]
str x12,[x0,#96]
str x13,[x0,#104]
str x14,[x0,#112]
str x15,[x0,#120]
str x16,[x0,#128]
str x17,[x0,#136]
str x18,[x0,#144]
str x19,[x0,#152]
str x20,[x0,#160]
str x21,[x0,#168]
str x22,[x0,#176]
str x23,[x0,#184]
str x24,[x0,#192]
str x25,[x0,#200]
str x26,[x0,#208]
str x27,[x0,#216]
str x28,[x0,#224]
str x29,[x0,#232]
str x30,[x0,#240]
adrp x0,detach_vector_got
add x0,x0,:lo12:detach_vector_got
str q0,[x0,#0]
str q1,[x0,#16]
str q2,[x0,#32]
str q3,[x0,#48]
str q4,[x0,#64]
str q5,[x0,#80]
str q6,[x0,#96]
str q7,[x0,#112]
str q8,[x0,#128]
str q9,[x0,#144]
str q10,[x0,#160]
str q11,[x0,#176]
str q12,[x0,#192]
str q13,[x0,#208]
str q14,[x0,#224]
str q15,[x0,#240]
str q16,[x0,#256]
str q17,[x0,#272]
str q18,[x0,#288]
str q19,[x0,#304]
str q20,[x0,#320]
str q21,[x0,#336]
str q22,[x0,#352]
str q23,[x0,#368]
str q24,[x0,#384]
str q25,[x0,#400]
str q26,[x0,#416]
str q27,[x0,#432]
str q28,[x0,#448]
str q29,[x0,#464]
str q30,[x0,#480]
str q31,[x0,#496]
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
ldr x0,[x0]
mov sp,x0
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
ldr x1,[x0]
msr nzcv,x1
ldr x1,[x0,#8]
msr fpcr,x1
ldr x1,[x0,#16]
msr fpsr,x1
ldp q14,q15,[sp],#32
ldp q12,q13,[sp],#32
ldp q10,q11,[sp],#32
ldp q8,q9,[sp],#32
ldp x29,x30,[sp],#16
ldp x27,x28,[sp],#16
ldp x25,x26,[sp],#16
ldp x23,x24,[sp],#16
ldp x21,x22,[sp],#16
ldp x19,x20,[sp],#16
ret
.globl detach_gprs_selfmod2_end
detach_gprs_selfmod2_end:
.size detach_gprs_selfmod2,.-detach_gprs_selfmod2
.p2align 16
.globl detach_flags_cache
.type detach_flags_cache,%function
detach_flags_cache:
stp x19,x20,[sp,#-16]!
stp x21,x22,[sp,#-16]!
stp x23,x24,[sp,#-16]!
stp x25,x26,[sp,#-16]!
stp x27,x28,[sp,#-16]!
stp x29,x30,[sp,#-16]!
stp q8,q9,[sp,#-32]!
stp q10,q11,[sp,#-32]!
stp q12,q13,[sp,#-32]!
stp q14,q15,[sp,#-32]!
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
mov x1,sp
str x1,[x0]
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
mov x0,#0xa0000000
msr nzcv,x0
mov x0,#0x07800000
msr fpcr,x0
mov x0,#0x9c
msr fpsr,x0
adrp x0,detach_ready
add x0,x0,:lo12:detach_ready
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
.Lflags_cache_loop:
adrp x0,detach_progress
add x0,x0,:lo12:detach_progress
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
adrp x0,detach_released
add x0,x0,:lo12:detach_released
ldrb w0,[x0]
cbz w0,.Lflags_cache_loop
adrp x0,detach_status
add x0,x0,:lo12:detach_status
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
ldr x0,[x0]
mov sp,x0
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
ldr x1,[x0]
msr nzcv,x1
ldr x1,[x0,#8]
msr fpcr,x1
ldr x1,[x0,#16]
msr fpsr,x1
ldp q14,q15,[sp],#32
ldp q12,q13,[sp],#32
ldp q10,q11,[sp],#32
ldp q8,q9,[sp],#32
ldp x29,x30,[sp],#16
ldp x27,x28,[sp],#16
ldp x25,x26,[sp],#16
ldp x23,x24,[sp],#16
ldp x21,x22,[sp],#16
ldp x19,x20,[sp],#16
ret
.globl detach_flags_cache_end
detach_flags_cache_end:
.size detach_flags_cache,.-detach_flags_cache
.p2align 16
.globl detach_flags_selfmod
.type detach_flags_selfmod,%function
detach_flags_selfmod:
stp x19,x20,[sp,#-16]!
stp x21,x22,[sp,#-16]!
stp x23,x24,[sp,#-16]!
stp x25,x26,[sp,#-16]!
stp x27,x28,[sp,#-16]!
stp x29,x30,[sp,#-16]!
stp q8,q9,[sp,#-32]!
stp q10,q11,[sp,#-32]!
stp q12,q13,[sp,#-32]!
stp q14,q15,[sp,#-32]!
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
mov x1,sp
str x1,[x0]
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
movz w2,#0
movk w2,#0x5280,lsl #16
mov x0,#0xa0000000
msr nzcv,x0
mov x0,#0x07800000
msr fpcr,x0
mov x0,#0x9c
msr fpsr,x0
adrp x0,detach_ready
add x0,x0,:lo12:detach_ready
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
.Lflags_selfmod_loop:
ubfx w0,w2,#5,#16
add w0,w0,#1
bfi w2,w0,#5,#16
adr x0,.Lflags_selfmod_patch
str w2,[x0]
dc cvau,x0
dsb ish
ic ivau,x0
dsb ish
isb
.Lflags_selfmod_patch:
movz w0,#0
adrp x0,detach_progress
add x0,x0,:lo12:detach_progress
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
adrp x0,detach_released
add x0,x0,:lo12:detach_released
ldrb w0,[x0]
cbz w0,.Lflags_selfmod_loop
adrp x0,detach_status
add x0,x0,:lo12:detach_status
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
ldr x0,[x0]
mov sp,x0
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
ldr x1,[x0]
msr nzcv,x1
ldr x1,[x0,#8]
msr fpcr,x1
ldr x1,[x0,#16]
msr fpsr,x1
ldp q14,q15,[sp],#32
ldp q12,q13,[sp],#32
ldp q10,q11,[sp],#32
ldp q8,q9,[sp],#32
ldp x29,x30,[sp],#16
ldp x27,x28,[sp],#16
ldp x25,x26,[sp],#16
ldp x23,x24,[sp],#16
ldp x21,x22,[sp],#16
ldp x19,x20,[sp],#16
ret
.globl detach_flags_selfmod_end
detach_flags_selfmod_end:
.size detach_flags_selfmod,.-detach_flags_selfmod
.p2align 16
.globl detach_sp_cache
.type detach_sp_cache,%function
detach_sp_cache:
stp x19,x20,[sp,#-16]!
stp x21,x22,[sp,#-16]!
stp x23,x24,[sp,#-16]!
stp x25,x26,[sp,#-16]!
stp x27,x28,[sp,#-16]!
stp x29,x30,[sp,#-16]!
stp q8,q9,[sp,#-32]!
stp q10,q11,[sp,#-32]!
stp q12,q13,[sp,#-32]!
stp q14,q15,[sp,#-32]!
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
mov x1,sp
str x1,[x0]
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
adrp x0,detach_safe_stack
add x0,x0,:lo12:detach_safe_stack
ldr x0,[x0]
mov sp,x0
adrp x0,detach_ready
add x0,x0,:lo12:detach_ready
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
.Lsp_cache_loop:
adrp x0,detach_progress
add x0,x0,:lo12:detach_progress
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
adrp x0,detach_released
add x0,x0,:lo12:detach_released
ldrb w0,[x0]
cmp w0,#1
b.ne .Lsp_cache_loop
adrp x0,detach_stack_observed
add x0,x0,:lo12:detach_stack_observed
mov x1,sp
str x1,[x0]
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
ldr x0,[x0]
mov sp,x0
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
ldr x1,[x0]
msr nzcv,x1
ldr x1,[x0,#8]
msr fpcr,x1
ldr x1,[x0,#16]
msr fpsr,x1
ldp q14,q15,[sp],#32
ldp q12,q13,[sp],#32
ldp q10,q11,[sp],#32
ldp q8,q9,[sp],#32
ldp x29,x30,[sp],#16
ldp x27,x28,[sp],#16
ldp x25,x26,[sp],#16
ldp x23,x24,[sp],#16
ldp x21,x22,[sp],#16
ldp x19,x20,[sp],#16
ret
.globl detach_sp_cache_end
detach_sp_cache_end:
.size detach_sp_cache,.-detach_sp_cache
.p2align 16
.globl detach_sp_selfmod
.type detach_sp_selfmod,%function
detach_sp_selfmod:
stp x19,x20,[sp,#-16]!
stp x21,x22,[sp,#-16]!
stp x23,x24,[sp,#-16]!
stp x25,x26,[sp,#-16]!
stp x27,x28,[sp,#-16]!
stp x29,x30,[sp,#-16]!
stp q8,q9,[sp,#-32]!
stp q10,q11,[sp,#-32]!
stp q12,q13,[sp,#-32]!
stp q14,q15,[sp,#-32]!
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
mov x1,sp
str x1,[x0]
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
mrs x1,nzcv
str x1,[x0]
mrs x1,fpcr
str x1,[x0,#8]
mrs x1,fpsr
str x1,[x0,#16]
adrp x0,detach_safe_stack
add x0,x0,:lo12:detach_safe_stack
ldr x0,[x0]
mov sp,x0
movz w2,#0
movk w2,#0x5280,lsl #16
adrp x0,detach_ready
add x0,x0,:lo12:detach_ready
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
.Lsp_selfmod_loop:
ubfx w0,w2,#5,#16
add w0,w0,#1
bfi w2,w0,#5,#16
adr x0,.Lsp_selfmod_patch
str w2,[x0]
dc cvau,x0
dsb ish
ic ivau,x0
dsb ish
isb
.Lsp_selfmod_patch:
movz w0,#0
adrp x0,detach_progress
add x0,x0,:lo12:detach_progress
str x1,[sp,#-16]!
mov w1,#1
str w1,[x0]
ldr x1,[sp],#16
adrp x0,detach_released
add x0,x0,:lo12:detach_released
ldrb w0,[x0]
cmp w0,#1
b.ne .Lsp_selfmod_loop
adrp x0,detach_stack_observed
add x0,x0,:lo12:detach_stack_observed
mov x1,sp
str x1,[x0]
adrp x0,detach_saved_stack
add x0,x0,:lo12:detach_saved_stack
ldr x0,[x0]
mov sp,x0
adrp x0,detach_original_fp
add x0,x0,:lo12:detach_original_fp
ldr x1,[x0]
msr nzcv,x1
ldr x1,[x0,#8]
msr fpcr,x1
ldr x1,[x0,#16]
msr fpsr,x1
ldp q14,q15,[sp],#32
ldp q12,q13,[sp],#32
ldp q10,q11,[sp],#32
ldp q8,q9,[sp],#32
ldp x29,x30,[sp],#16
ldp x27,x28,[sp],#16
ldp x25,x26,[sp],#16
ldp x23,x24,[sp],#16
ldp x21,x22,[sp],#16
ldp x19,x20,[sp],#16
ret
.globl detach_sp_selfmod_end
detach_sp_selfmod_end:
.size detach_sp_selfmod,.-detach_sp_selfmod
.p2align 16
#else
#error Linux x86_64/AArch64 only
#endif
.section .note.GNU-stack,"",%progbits
