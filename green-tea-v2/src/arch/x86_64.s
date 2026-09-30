.global _swap_context
.global swap_context
_swap_context:
swap_context:
    // Save current registers into `from` (*mut Context in rdi)
    mov [rdi + 0x00], rsp
    mov [rdi + 0x08], r15
    mov [rdi + 0x10], r14
    mov [rdi + 0x18], r13
    mov [rdi + 0x20], r12
    mov [rdi + 0x28], rbx
    mov [rdi + 0x30], rbp

    // Load saved registers from `to` (*const Context in rsi)
    mov rsp, [rsi + 0x00]
    mov r15, [rsi + 0x08]
    mov r14, [rsi + 0x10]
    mov r13, [rsi + 0x18]
    mov r12, [rsi + 0x20]
    mov rbx, [rsi + 0x28]
    mov rbp, [rsi + 0x30]

    ret // pops the value at the top of the stack, jumps to it think 'pop rip'
        // The value was put there by `call` which in essence is 'push rip'
.global _bootstrap_entry
.global bootstrap_entry
_bootstrap_entry:
bootstrap_entry:
    mov rdi, r15 // entry argument from r15 -> first argument
    and rsp, -16 // SysV ABI: rsp must be 16-byte aligned right before a call
    call r14 // call function pointer
    ud2 // trigger invalid opcode execution
