// Linux AArch64, libc-free: a faulting STXR must preserve the old full X20.
// Linux 6.12.112 UAPI asm/ucontext.h: flags + link + stack_t = 40 bytes,
// sigmask plus padding = 128 bytes; aligned sigcontext therefore starts at 176.
// asm/sigcontext.h: fault_address, regs[31], sp, pc, pstate.
.equ UC_X20, 176 + 8 + 20 * 8
.equ UC_REGS, 176 + 8
.equ UC_PC, 176 + 8 + 31 * 8 + 8
.equ MARKER_LO, 0x5678
.equ MARKER_HI, 0x1234

.macro marker reg
    movz \reg, #MARKER_LO
    movk \reg, #MARKER_HI, lsl #16
    movk \reg, #0x3344, lsl #32
    movk \reg, #0x1122, lsl #48
.endm

.macro sentinel reg, value
    movz \reg, #\value
    movk \reg, #\value, lsl #16
    movk \reg, #\value, lsl #32
    movk \reg, #\value, lsl #48
.endm

// Assemble with --defsym PAIR=1 to force the low-register STXP case:
// X0/X1 data, W2 status, X3 base, with live sentinels in X4 through X7.
// STOLEN=1 instead exercises the translator's reserved X28 as store status.
.ifdef PAIR
.equ UC_STATUS, UC_REGS + 2 * 8
.else
.ifdef STOLEN
.equ UC_STATUS, UC_REGS + 28 * 8
status_x .req x28
status_w .req w28
.else
.equ UC_STATUS, UC_X20
status_x .req x20
status_w .req w20
.endif
.endif

.global _start
.text
_start:
    mov x0, #0
    mov x1, #4096
    mov x2, #3                  // PROT_READ | PROT_WRITE
    mov x3, #0x22               // MAP_PRIVATE | MAP_ANONYMOUS
    mov x4, #-1
    mov x5, #0
    mov x8, #222                // mmap
    svc #0
    tbnz x0, #63, bad_mmap
    mov x19, x0
    mov w1, #0x55
    str x1, [x19]
    mov x1, #0x66
    str x1, [x19, #8]
    mov x1, #4096
    mov x2, #1                  // PROT_READ
    mov x8, #226                // mprotect
    svc #0
    cbnz x0, bad_mprotect

    mov x0, #11                 // SIGSEGV
    adrp x1, action
    add x1, x1, :lo12:action
    mov x2, #0
    mov x3, #8                  // kernel sigset_t size
    mov x8, #134                // rt_sigaction
    svc #0
    cbnz x0, bad_sigaction
    adrp x22, handler_count
    add x22, x22, :lo12:handler_count
    mov x23, #1000              // bound native monitor-loss retries
retry:
.ifdef PAIR
    marker x2
    mov x3, x19
    sentinel x4, 0x4444
    sentinel x5, 0x5555
    sentinel x6, 0x6666
    sentinel x7, 0x7777
    ldxp x0, x1, [x3]
faulting_store:
    stxp w2, x0, x1, [x3]
.else
    marker status_x            // full original status, including upper bits
    mov w1, #0xaa
    ldxr w0, [x19]
faulting_store:
    stxr status_w, w1, [x19]
.endif
    ldr x0, [x22]
    cbnz x0, handled
    subs x23, x23, #1
    b.ne retry
    mov x0, #4
    b exit
handled:
    cmp x0, #1
    b.ne bad_count
    marker x1
.ifdef PAIR
    cmp x2, x1
.else
    cmp status_x, x1
.endif
    b.ne bad_status
.ifdef PAIR
    sentinel x1, 0x4444
    cmp x4, x1
    b.ne bad_x4
    sentinel x1, 0x5555
    cmp x5, x1
    b.ne bad_x5
    sentinel x1, 0x6666
    cmp x6, x1
    b.ne bad_x6
    sentinel x1, 0x7777
    cmp x7, x1
    b.ne bad_x7
.endif
    ldr x0, [x19]
    cmp x0, #0x55
    b.ne bad_memory
    ldr x0, [x19, #8]
    cmp x0, #0x66
    b.ne bad_memory
    mov x0, #0
    b exit

handler:
    cmp x0, #11
    b.ne bad_signal
    ldr x3, [x2, #UC_PC]
    adrp x4, faulting_store
    add x4, x4, :lo12:faulting_store
    cmp x3, x4
    b.ne bad_pc
    ldr x5, [x2, #UC_STATUS]     // X2 is the ucontext pointer, not app X2
    marker x6
    cmp x5, x6
    b.ne bad_saved_status
    ldr x5, [x2, #UC_REGS]      // the original load put 0x55 in application X0
    cmp x5, #0x55
    b.ne bad_x0
.ifdef PAIR
    ldr x8, [x2, #(UC_REGS + 4 * 8)]
    sentinel x9, 0x4444
    cmp x8, x9
    b.ne bad_x4
    ldr x8, [x2, #(UC_REGS + 5 * 8)]
    sentinel x9, 0x5555
    cmp x8, x9
    b.ne bad_x5
    ldr x8, [x2, #(UC_REGS + 6 * 8)]
    sentinel x9, 0x6666
    cmp x8, x9
    b.ne bad_x6
    ldr x8, [x2, #(UC_REGS + 7 * 8)]
    sentinel x9, 0x7777
    cmp x8, x9
    b.ne bad_x7
.endif
    ldr x5, [x19]
    cmp x5, #0x55
    b.ne bad_handler_memory
    ldr x5, [x19, #8]
    cmp x5, #0x66
    b.ne bad_handler_memory
    ldr x5, [x22]
    cbnz x5, bad_handler_count
    add x5, x5, #1
    str x5, [x22]
    add x3, x3, #4
    str x3, [x2, #UC_PC]         // skip the faulting store on sigreturn
    ret
restorer:
    mov x8, #139                // rt_sigreturn
    svc #0

bad_mmap:           mov x0, #1; b exit
bad_mprotect:       mov x0, #2; b exit
bad_sigaction:      mov x0, #3; b exit
bad_count:          mov x0, #5; b exit
bad_status:         mov x0, #6; b exit
bad_memory:         mov x0, #7; b exit
bad_signal:         mov x0, #8; b exit
bad_pc:             mov x0, #9; b exit
bad_saved_status:   mov x0, #10; b exit
bad_handler_memory: mov x0, #11; b exit
bad_handler_count:  mov x0, #12; b exit
bad_x4:             mov x0, #13; b exit
bad_x5:             mov x0, #14; b exit
bad_x6:             mov x0, #15; b exit
bad_x7:             mov x0, #16; b exit
bad_x0:             mov x0, #17
exit:
    mov x8, #93
    svc #0

.data
.balign 8
action:
    .quad handler
    .quad 0x04000004            // SA_RESTORER | SA_SIGINFO
    .quad restorer
    .quad 0
handler_count:
    .quad 0
.section .note.GNU-stack,"",@progbits
