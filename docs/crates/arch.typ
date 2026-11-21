#import "../lib.typ": *

= green-tea-arch <crate-arch>

`green-tea-arch` saves one execution's registers, loads another's, and prepares a stack that has never run, on x86_64, aarch64 and riscv64. It is `no_std`, makes no system calls and has no dependencies, so the same code serves programs under Linux and macOS and the kernels. It decides nothing: who owns the stacks and when to switch belong to the caller.

#spec(
  ("Source", [#repo("green-tea-arch/src/"), 188 lines of Rust and 153 of assembly]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-v2`, `green-tea-bare`]),
  ("Tests", [none of its own; `green-tea-v2`'s tests under `cargo xtask test`, and the kernels]),
)

== Public interface

The crate root selects one module with `cfg(target_arch)` and re-exports it. Any other architecture is a `compile_error!`. Each of the modules `x86_64`, `aarch64` and `riscv64` provides the same four items:

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Context`], [The saved registers. `repr(C)`, with fields private, and `Default` gives all zeroes],
  [`Context::new(top, entry, arg)`], [`unsafe`. A context whose first switch calls `entry(arg)` on the stack ending at `top`],
  [`Entry`], [`extern "C" fn(arg: *mut ()) -> !`: the function a fresh context starts in. It has nothing to return to],
  [`swap_context(from, to)`], [`unsafe`, in assembly. Saves the running registers into `from`, loads `to`'s, and continues wherever `to` left off],
)

== What a switch has to save

To the compiler, `swap_context` is an ordinary external function. Each ABI divides the registers in two: a function may destroy the *caller-saved* ones, so the compiler has already saved any it needs before the call, and must preserve the *callee-saved* ones. A switch therefore has to save only the callee-saved set, the stack pointer, and the place to resume.

#dtable(columns: (auto, 1fr, 1fr, 1fr), ("", "x86_64 (System V)", "aarch64 (AAPCS64)", "riscv64 (LP64D)"),
  [Integer], [`rbx rbp r12`–`r15`], [`x19`–`x28`, `x29` (frame)], [`s0`–`s11`],
  [Floating point], [none], [`d8`–`d15`], [`fs0`–`fs11`],
  [Return address], [on the stack], [`x30`], [`ra`],
  [`Context` size], [56 bytes], [168 bytes], [208 bytes],
  [Instructions in the switch], [15], [25], [53],
  [First argument], [`rdi`], [`x0`], [`a0`],
)

The sizes on aarch64 and riscv64 are checked at compile time by a `const` assertion, since the assembly hard-codes the offsets.

== x86_64

=== The switch

`rdi` holds `from` and `rsi` holds `to`. Seven stores and seven loads move the registers, in the order of the fields of `Context`:

#srcfile("green-tea-arch/src/x86_64.s", lines: ((4, 24),), notes: ((6, [`rsp` first. It points at the return address that the `call` to `swap_context` pushed, so saving it also saves where this execution resumes.]), (7, [`r15`, `r14`, `r13`, `r12`, `rbx` and `rbp`: the callee-saved registers, at offsets `0x08` to `0x30`.]), (15, [From this instruction on, the code is running on the other stack.]), (23, [Pops the top of the new stack into the instruction pointer.]),))[```text
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
```]

`rip` isn't in `Context`. Every suspended execution has its resume address on top of its own stack, and `ret` is the instruction that consumes it. A task that called `swap_context` to yield resumes on the line after that call, with `rsp` back where the call found it.

=== Priming a fresh stack

A task that has never run has no `call` on its stack, so `Context::new` writes the return address itself:

#srcfile("green-tea-arch/src/x86_64.rs", lines: ((35, 35), (41, 50),), notes: ((41, [Round `top` down to a multiple of 16.]), (42, [24 bytes below it: an address ≡ 8 (mod 16).]), (43, [The one word written to the new stack.]), (46, [Carried in a callee-saved register, because the switch loads only those.]),))[```rust
pub unsafe fn new(top: *mut u8, entry: Entry, arg: *mut ()) -> Context {
// …
    let top = (top as usize) & !0xF;
    let sp = (top - 24) as *mut u64;
    unsafe { sp.write(bootstrap_entry as *const () as u64) };
    Context {
        rsp: sp as u64,
        r15: arg as u64,
        r14: entry as *const () as u64,
        ..Context::default()
    }
}
```]

#bytefield(caption: [The top of a fresh x86_64 stack, low addresses on the left. `rsp` points at the first field],
  ("bootstrap_entry", "top − 24", 3), ("unused", "top − 16", 2), ("unused", "top − 8", 2))

The value of 24 follows from the System V rule: at a function's first instruction, `rsp + 8` is a multiple of 16. The sequence is:

+ `ret` pops the slot at `top − 24`, leaving `rsp` at `top − 16`, a multiple of 16.
+ `bootstrap_entry` moves the argument into `rdi`. Its `and rsp, -16` changes nothing on this path; the source calls it belt and braces.
+ `call r14` pushes its return address at `top − 24`, in the same slot, and enters the function with `rsp` ≡ 8 (mod 16).

#srcfile("green-tea-arch/src/x86_64.s", lines: ((28, 32),))[```text
bootstrap_entry:
    mov rdi, r15 // entry argument from r15 -> first argument
    and rsp, -16 // SysV ABI: rsp must be 16-byte aligned right before a call
    call r14 // call function pointer
    ud2 // trigger invalid opcode execution
```]

`ud2` is an instruction defined to be invalid. `Entry` promises never to return, and if one did, the process would stop there instead of running whatever bytes follow.

The debugger confirms each step on the first switch of `coop-n-v2`. At the breakpoint, `rsi` is the fresh context:

#output(title: "gdb target/debug/coop-n-v2, stopped at the first swap_context")[
```
--- the context of a fresh task (rsi): rsp r15 r14 r13 r12 rbx rbp
0x55555564da40:	0x00007ffff7fb9fe8	0x000055555564da40
0x55555564da50:	0x00005555555ea3f0	0x0000000000000000
0x55555564da60:	0x0000000000000000	0x0000000000000000
0x55555564da70:	0x0000000000000000
--- the slot rsp points to
0x7ffff7fb9fe8:	0x5555555fa893 <bootstrap_entry>
--- after 15 instructions
=> 0x5555555fa893 <bootstrap_entry>:	mov    rdi,r15
rsp mod 16 = 0
--- after 3 more
=> 0x5555555ea3f0 <_RNvNtCs4znrrNFW9Hp_12green_tea_v24task10task_entry>:	sub    rsp,0x78
rsp mod 16 = 8
rdi == r15: 1
0x7ffff7fb9fe8:	0x5555555fa89d <bootstrap_entry+10>
```
]

The saved `rsp` ends in `fe8`, 24 bytes below the page boundary at `0x7ffff7fba000`. The last line reads the same slot after the `call`: it held `bootstrap_entry`, and now holds the return address inside it. `r13`, `r12`, `rbx` and `rbp` start at zero, and a zero `rbp` ends a frame-pointer backtrace.

== aarch64

=== The switch

`bl` puts the return address in the link register, `x30`, and writes nothing to memory. So `x30` is saved like any other register, and `ret` jumps to it:

#srcfile("green-tea-arch/src/aarch64.s", lines: ((7, 20), (23, 37),), notes: ((9, [`sp` can't be the source of a `str`, so it goes through `x2`, a caller-saved scratch register.]), (11, [`stp` stores a pair in one instruction: `x19` to `x28`, then `x29` and `x30`.]), (17, [The low 64 bits of `v8`–`v15`, which are all AAPCS64 asks a callee to preserve.]), (24, [From here on, the stack is the other task's.]), (30, [Loads `to`'s resume address into `x30`.]), (36, [Branches to `x30`. The stack is not read.]),))[```text
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
// …
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
```]

The file is written to assemble for both ELF and Mach-O, whose C symbols carry a leading underscore, so each symbol has two labels. `.p2align 2` aligns the code to the architecture's 4-byte instructions.

=== Priming a fresh stack

Nothing is written to the stack. All the state of a fresh context fits in `Context`:

#srcfile("green-tea-arch/src/aarch64.rs", lines: ((45, 45), (49, 57),), notes: ((51, [AAPCS64 requires `sp` to be 16-byte aligned at all times, not only at calls.]), (52, [`x19` carries the argument.]), (53, [`x21` carries the function.]), (54, [The first `ret` goes here.]),))[```rust
pub unsafe fn new(top: *mut u8, entry: Entry, arg: *mut ()) -> Context {
// …
    let top = (top as usize) & !0xF;
    Context {
        sp: top as u64,
        x19: arg as u64,
        x21: entry as *const () as u64,
        x30: bootstrap_entry as *const () as u64,
        ..Context::default() // x29 = 0 ends frame-pointer backtraces here
    }
}
```]

#bytefield(caption: [`Context` on aarch64, with the fields a fresh context sets in the first, second, fourth and thirteenth words],
  ("sp", "0", 2), ("x19", "8", 2), ("x20", "16", 1), ("x21", "24", 2), ("x22–x29", "32–88", 3), ("x30", "96", 2), ("d8–d15", "104–160", 3))

#srcfile("green-tea-arch/src/aarch64.s", lines: ((43, 46),), notes: ((45, [`blr` writes the address of the next instruction to `x30` and branches.]), (46, [A breakpoint trap, in case the function returns.]),))[```text
bootstrap_entry:
    mov x0, x19 // entry argument from x19 -> first argument
    blr x21     // call the entry function
    brk #0      // trap: the entry function must never return
```]

No alignment step is needed: `sp` is already aligned and neither `ret` nor `blr` moves it.

#output(title: "gdb-multiarch, coop-n-v2 under qemu-aarch64, at the first swap_context")[
```
--- the context of a fresh task (x1): sp x19 x20 x21
0x4000012a5030:	0x00004000013aa000	0x00004000012a5030
0x4000012a5040:	0x0000000000000000	0x00007445d7d4e820
--- x30, at offset 96
0x4000012a5090:	0x7445d7d5f27c <bootstrap_entry>
--- after 25 instructions
=> 0x7445d7d5f27c <bootstrap_entry>:	mov	x0, x19
sp mod 16 = 0
--- after 2 more
=> 0x7445d7d4e820 <_RNvNtCs1z9GvXhq2LS_12green_tea_v24task10task_entry>:	sub	sp, sp, #0x90
sp mod 16 = 0
x0 == x19: 1
bootstrap_entry + 8 in section .text of
```
]

`sp` is the page-aligned top of the stack, unchanged by the switch and by the call. `x30` afterwards is `bootstrap_entry + 8`, the `brk` that follows the `blr`.

== riscv64

=== The switch

RISC-V has no load-pair instruction, so each register takes one `sd` and one `ld`: 26 stores, 26 loads and a `ret`. The listing cuts the runs of identical lines:

#srcfile("green-tea-arch/src/riscv64.s", lines: ((5, 6), (9, 14), (24, 26), (36, 36), (39, 42), (52, 54), (64, 66),), notes: ((6, [`rustc` doesn't always pass the target's float extensions to `global_asm!`, and `fsd` needs D. The file turns it on for itself.]), (11, [`sp` is an ordinary register here and can be stored directly.]), (12, [`ra` holds the return address, as `x30` does on aarch64.]), (25, [`fs0`–`fs11`, 12 double-precision registers, from offset 112 to 200.]), (39, [The stack changes here.]), (66, [A pseudo-instruction: `jalr zero, 0(ra)`.]),))[```text
.option push
.option arch, +d
// …
swap_context:
    # Save current registers into `from` (*mut Context in a0)
    sd   sp,    0(a0)
    sd   ra,    8(a0)   # return address: where our caller resumes
    sd   s0,   16(a0)   # s0 is also the frame pointer
    sd   s1,   24(a0)
// …
    sd   s11, 104(a0)
    fsd  fs0, 112(a0)
    fsd  fs1, 120(a0)
// …
    fsd  fs11, 200(a0)
// …
    ld   sp,    0(a1)
    ld   ra,    8(a1)
    ld   s0,   16(a1)
    ld   s1,   24(a1)
// …
    ld   s11, 104(a1)
    fld  fs0, 112(a1)
    fld  fs1, 120(a1)
// …
    fld  fs11, 200(a1)

    ret     # jalr zero, 0(ra): like aarch64, the return address is in a register
```]

`gp` and `tp` aren't saved. Neither is callee-saved in the psABI: `gp` is fixed for the program, `tp` belongs to the thread, and all the tasks of a thread share both.

=== Priming a fresh stack

Priming has the shape it has on aarch64, with different register numbers:

#srcfile("green-tea-arch/src/riscv64.rs", lines: ((40, 40), (43, 51),), notes: ((45, [The LP64D psABI requires a 16-byte aligned `sp`.]), (46, [The first `ret` goes here.]), (47, [`s1` carries the argument.]), (48, [`s2` carries the function.]),))[```rust
pub unsafe fn new(top: *mut u8, entry: Entry, arg: *mut ()) -> Context {
// …
    let top = (top as usize) & !0xF;
    Context {
        sp: top as u64,
        ra: bootstrap_entry as *const () as u64,
        s1: arg as u64,
        s2: entry as *const () as u64,
        ..Context::default() // s0 = 0 ends frame-pointer backtraces here
    }
}
```]

#srcfile("green-tea-arch/src/riscv64.s", lines: ((70, 73),), notes: ((72, [Short for `jalr ra, 0(s2)`: links into `ra` and jumps.]), (73, [A defined illegal instruction, in case the function returns.]),))[```text
bootstrap_entry:
    mv   a0, s1 # entry argument from s1 -> first argument
    jalr s2     # call the entry function (links into ra)
    unimp       # trap: the entry function must never return
```]

#output(title: "gdb-multiarch, coop-n-v2 under qemu-riscv64, at the first swap_context")[
```
--- the context of a fresh task (a1): sp ra s0 s1 s2
0x55555561f030:	0x00002aaaabca5000	0x000055555559d5a8
0x55555561f040:	0x0000000000000000	0x000055555561f030
0x55555561f050:	0x0000555555592a16
--- ra, at offset 8
0x55555561f038:	0x55555559d5a8 <bootstrap_entry>
--- after 53 instructions
=> 0x55555559d5a8 <bootstrap_entry>:	mv	a0,s1
sp mod 16 = 0
--- after 2 more
=> 0x555555592a16 <_RNvNtCsinEvBbMGGtt_12green_tea_v24task10task_entry>:	addi	sp,sp,-144
sp mod 16 = 0
a0 == s1: 1
bootstrap_entry + 4 in section .text of
```
]

On all three architectures the argument equals the address of the context being loaded, because `green-tea-v2` passes its task record and `rustc` happened to place `context` first in it.

== How it is exercised

The crate has no tests of its own, since a switch means nothing without a stack and a caller. `cargo xtask test` runs `green-tea-v2`'s 13 tests and both demos on each architecture, in debug and release, using QEMU's user mode for the two cross targets. For Apple Silicon it only builds the library, which proves that the assembly assembles for Mach-O:

#output(title: "cargo xtask test")[
```
=== summary
ok    host                         release test
ok    aarch64-unknown-linux-gnu    release test
ok    riscv64gc-unknown-linux-gnu  release test
ok    aarch64-apple-darwin         release build lib
```
]

The kernels call the same `Context::new` and `swap_context` (@crate-bare).

== Limits

- *The floating-point control state is not switched.* `mxcsr` and the x87 control word, `fpcr`, and `fcsr` stay in the CPU, so a rounding mode one task sets applies to the next task that runs. A small program outside the crate shows it on all three, through C's `fesetround`:

#output(title: "task A sets a rounding mode and yields, task B reads it")[
```
x86_64: at the start, rounding mode 0
x86_64: task A sets rounding mode 2048
x86_64: task B finds rounding mode 2048
aarch64: task B finds rounding mode 4194304
riscv64: task B finds rounding mode 3
```
]

- The assembly has no unwind information, so a debugger or an unwinder can't walk past `bootstrap_entry`. @crate-v2 shows what that does to a panic.
- The registers of SIMD and vector extensions beyond the callee-saved sets above aren't saved, which the three ABIs allow.
- The comment on `ret` in `x86_64.s` says the return address "was put there by `call`". On a first switch `Context::new` put it there.
- `Context::new` trusts `top`. Nothing checks that the stack is writable or large enough.

Three register sets, one contract. @crate-v2 builds tasks on it.
