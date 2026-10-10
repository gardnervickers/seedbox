.global _start
.text
_start:
    mov w6, #1000
retry:
    adrp x3, value
    add x3, x3, :lo12:value
.ifdef DISTINCT
    ldaxr w5, [x3]
    cbnz w5, unexpected
.else
    ldaxr w3, [x3]
    cbnz w3, unexpected
.endif
    adrp x4, value
    add x4, x4, :lo12:value
    mov w1, #1
    stlxr w2, w1, [x4]
    cbz w2, success
    subs w6, w6, #1
    b.ne retry
    mov w0, #1
    b done
success:
    mov w0, #0
    b done
unexpected:
    mov w0, #2
done:
    mov x8, #93
    svc #0
.data
.balign 8
value: .quad 0

.section .note.GNU-stack,"",@progbits
