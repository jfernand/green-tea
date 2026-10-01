# RV64 LP64D psABI. `#` is the comment character in RISC-V assembly.

# Enable the D extension for this file. rustc doesn't always pass the target's float
# extensions to global_asm (release builds assemble it without them), and fsd/fld need D.
.option push
.option arch, +d
.p2align 2
.global swap_context
swap_context:
    # Save current registers into `from` (*mut Context in a0)
    sd   sp,    0(a0)
    sd   ra,    8(a0)   # return address: where our caller resumes
    sd   s0,   16(a0)   # s0 is also the frame pointer
    sd   s1,   24(a0)
    sd   s2,   32(a0)
    sd   s3,   40(a0)
    sd   s4,   48(a0)
    sd   s5,   56(a0)
    sd   s6,   64(a0)
    sd   s7,   72(a0)
    sd   s8,   80(a0)
    sd   s9,   88(a0)
    sd   s10,  96(a0)
    sd   s11, 104(a0)
    fsd  fs0, 112(a0)
    fsd  fs1, 120(a0)
    fsd  fs2, 128(a0)
    fsd  fs3, 136(a0)
    fsd  fs4, 144(a0)
    fsd  fs5, 152(a0)
    fsd  fs6, 160(a0)
    fsd  fs7, 168(a0)
    fsd  fs8, 176(a0)
    fsd  fs9, 184(a0)
    fsd  fs10, 192(a0)
    fsd  fs11, 200(a0)

    # Load saved registers from `to` (*const Context in a1)
    ld   sp,    0(a1)
    ld   ra,    8(a1)
    ld   s0,   16(a1)
    ld   s1,   24(a1)
    ld   s2,   32(a1)
    ld   s3,   40(a1)
    ld   s4,   48(a1)
    ld   s5,   56(a1)
    ld   s6,   64(a1)
    ld   s7,   72(a1)
    ld   s8,   80(a1)
    ld   s9,   88(a1)
    ld   s10,  96(a1)
    ld   s11, 104(a1)
    fld  fs0, 112(a1)
    fld  fs1, 120(a1)
    fld  fs2, 128(a1)
    fld  fs3, 136(a1)
    fld  fs4, 144(a1)
    fld  fs5, 152(a1)
    fld  fs6, 160(a1)
    fld  fs7, 168(a1)
    fld  fs8, 176(a1)
    fld  fs9, 184(a1)
    fld  fs10, 192(a1)
    fld  fs11, 200(a1)

    ret     # jalr zero, 0(ra): like aarch64, the return address is in a register

.p2align 2
.global bootstrap_entry
bootstrap_entry:
    mv   a0, s1 # entry argument from s1 -> first argument
    jalr s2     # call the entry function (links into ra)
    unimp       # trap: the entry function must never return

.option pop
