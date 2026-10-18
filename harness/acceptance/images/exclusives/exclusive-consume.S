.global _start
.text
_start:
    adrp x4, value
    add x4, x4, :lo12:value
    mov w1, #0
    str w1, [x4]
    mov w6, #1000
retry:
    ldaxr w5, [x4]
    cbnz w5, bad
    stlxr w2, w1, [x4]
    cbz w2, second
    subs w6, w6, #1
    b.ne retry
bad:
    mov w0, #2
    b done
second:
    mov w1, #7
    stlxr w2, w1, [x4]
    eor w0, w2, #1
    // exit 0 means second store correctly failed; 1 means it wrongly succeeded.
done:
    mov x8, #93
    svc #0
.data
.balign 8
value: .quad 0

.section .note.GNU-stack,"",@progbits
