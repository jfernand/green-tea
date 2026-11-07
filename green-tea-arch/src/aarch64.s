// AAPCS64. Mach-O (macOS) prefixes C symbols with an underscore, ELF (Linux) doesn't,
// so each symbol gets both labels.
.p2align 2
.global _swap_context
.global swap_context
_swap_context:
swap_context:
    // Save current registers into `from` (*mut Context in x0)
    mov x2, sp              // sp can't be stored directly
    str x2,       [x0, #0]
    stp x19, x20, [x0, #8]
    stp x21, x22, [x0, #24]
    stp x23, x24, [x0, #40]
    stp x25, x26, [x0, #56]
    stp x27, x28, [x0, #72]
    stp x29, x30, [x0, #88] // frame pointer, and lr: where our caller resumes
    stp d8,  d9,  [x0, #104]
    stp d10, d11, [x0, #120]
    stp d12, d13, [x0, #136]
    stp d14, d15, [x0, #152]

    // Load saved registers from `to` (*const Context in x1)
    ldr x2,       [x1, #0]
    mov sp, x2
    ldp x19, x20, [x1, #8]
    ldp x21, x22, [x1, #24]
    ldp x23, x24, [x1, #40]
    ldp x25, x26, [x1, #56]
    ldp x27, x28, [x1, #72]
    ldp x29, x30, [x1, #88]
    ldp d8,  d9,  [x1, #104]
    ldp d10, d11, [x1, #120]
    ldp d12, d13, [x1, #136]
    ldp d14, d15, [x1, #152]

    ret // jumps to x30. Unlike x86 there's no return address on the stack: `bl` put
        // it in the link register, and we just swapped that for `to`'s.

.p2align 2
.global _bootstrap_entry
.global bootstrap_entry
_bootstrap_entry:
bootstrap_entry:
    mov x0, x19 // entry argument from x19 -> first argument
    blr x21     // call the entry function
    brk #0      // trap: the entry function must never return
