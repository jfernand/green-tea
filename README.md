# green-tea

[![CI](https://github.com/jfernand/green-tea/actions/workflows/ci.yml/badge.svg)](https://github.com/jfernand/green-tea/actions/workflows/ci.yml)

Green threads (user-space cooperative tasks) in Rust, built from scratch to learn how
context switching works on x86_64, aarch64 and riscv64.

Write-up: [Multitasking](https://jotafernand.casaroja.es/posts/019-multitasking/)

Inspired by https://dzania.github.io/green-threads-from-scratch/

With minimal help from R. Claude and R. Junie

## Layout

The workspace has two versions side by side, so you can compare them, and two bare-metal
kernels built on the same context switch:

| crate | what it shows |
|---|---|
| [`green-tea-v1`](green-tea-v1) | The first working version, x86_64 only. One global `MAIN` context, `static mut` state, and a `VecDeque` `run()` scheduler. Its git history goes through the bugs: a shadowed `static`, a missing `done` flag, and a stack misaligned for the SysV ABI. |
| [`green-tea-v2`](green-tea-v2) | The same ideas split into layers: `arch` (save/restore registers, prime a stack) → `Task` (an asymmetric coroutine that switches back to whoever resumed it) → `run`/`spawn` (a round-robin scheduler on a thread-local run queue). No `static mut`. Runs on x86_64, aarch64 (including Apple Silicon) and riscv64. |
| [`green-tea-arch`](green-tea-arch) | The `arch` layer on its own: `Context` and `swap_context` for each architecture. `no_std`, so v2 and the kernels share it. |
| [`green-tea-bare`](green-tea-bare) | What the kernels share: boot code and drivers per board behind the `Platform` and `TimerInterrupt` traits, a port of v2's `Task` and scheduler, a heap, and drivers for the serial port, QEMU's fw_cfg device and the ramfb display. |
| [`green-tea-coop`](green-tea-coop) | A bare-metal kernel for QEMU on riscv64, aarch64 and x86_64 whose tasks yield to each other, so you can watch the context switches on screen. |
| [`green-tea-preempt`](green-tea-preempt) | The same, but the tasks never yield: a timer interrupt preempts them. riscv64 only, so far. |
| [`xtask`](xtask) | `cargo xtask test` runs v2 across targets under qemu; `cargo xtask kernel` boots both kernels everywhere they run. |

Each version has two demos: `coop-2-*` drives two tasks with a hand-written loop, and
`coop-n-*` uses the scheduler.

```sh
cargo run --bin coop-n-v2
```

## How a context switch works

`swap_context(from, to)` stores the stack pointer and the callee-saved registers into
`from`, loads `to`'s, and returns, but onto `to`'s stack. To the compiler it is an
ordinary function call, so the caller-saved registers have already been spilled.

A new task's context is primed so that the first switch "returns" into a small
trampoline, `bootstrap_entry`, which calls the task's entry function.

| | x86_64 | aarch64 | riscv64 |
|---|---|---|---|
| Return address | pushed on the stack by `call` | `x30` (link register) | `ra` |
| Callee-saved integer registers | `rbx rbp r12–r15` | `x19–x28`, `x29` (fp) | `s0–s11` |
| Callee-saved float registers | none | `d8–d15` | `fs0–fs11` |
| Stack alignment | 16 bytes before a `call` | 16 bytes, always | 16 bytes |

The assembly is in [`green-tea-arch/src`](green-tea-arch/src).

## Watching switches on bare metal

```sh
sudo apt install qemu-system-misc qemu-system-arm qemu-system-x86
rustup target add riscv64gc-unknown-none-elf aarch64-unknown-none x86_64-unknown-none
cargo xtask kernel                    # one QEMU window per kernel and architecture, all at once
cargo xtask kernel --kernel preempt   # just the preemptive kernel
cargo xtask kernel --target aarch64   # just one architecture
cargo xtask kernel --headless --screenshot   # no windows; saves target/kernel/<kernel>-<arch>.png
```

### Cooperative: green-tea-coop

The kernel runs with no OS, no interrupts and one CPU. It gives QEMU's ramfb device a
framebuffer by writing its address and shape to the fw_cfg file `etc/ramfb`. Then five
tasks each own a column: every time one is resumed, it grows its bar by one step and
yields. Before each resume, the scheduler outlines the column of the task about to run and
bumps the switch counter at the top. When all tasks finish, the status bar turns green and
the serial console prints the total, which should be 57; `cargo xtask kernel` checks it.

Everything but [`green-tea-bare/src/platform`](green-tea-bare/src/platform) is shared.
Per architecture:

| | riscv64 (`virt`) | aarch64 (`virt`) | x86_64 (`q35`) |
|---|---|---|---|
| Boot | OpenSBI jumps to S-mode | QEMU jumps to EL1, MMU off | PVH: 32-bit mode, paging off; `_start` builds page tables and enters long mode |
| FPU | `sstatus.FS` on | `CPACR_EL1.FPEN` on | none (soft-float target) |
| Serial | 16550, memory-mapped | PL011 | 16550 on I/O ports |
| fw_cfg | memory-mapped | memory-mapped | I/O ports |
| Timer | `time` CSR | `cntvct_el0` | HPET |

Differences from v2: `thread_local!` becomes a plain static (there's one CPU), and stacks
come from a bump allocator with no guard page (with no paging, nothing can fault).

### Preemptive: green-tea-preempt

Same screen, but the workers grow their bars in a busy loop and never call `yield_now`.
Before resuming a task, the scheduler sets a timer deadline 100 ms ahead (through
OpenSBI's `sbi_set_timer`). When it passes, the CPU traps into a handler that saves *every*
register on the task's stack, then calls the same `yield_now` a cooperative task would.
When the task is next resumed, `swap_context` returns into the handler, which restores the
registers and `sret`s back into the loop. Each band of shading in a bar is one time slice.

`swap_context` itself is unchanged: it only saves callee-saved registers because, to the
compiler, it's a function call. An interrupt isn't, so the trap frame covers the rest.
The other rule is that the scheduler runs with interrupts off, and tasks with them on.

The kernel needs a board that implements `TimerInterrupt`, which only riscv64 does so
far. Building it for another target fails on that trait bound.

## Testing

```sh
cargo test               # host: the default members (v1 needs x86_64)
cargo xtask test         # v2 on host, aarch64 and riscv64 (qemu), debug and release
```

`cargo xtask test` needs qemu's user-mode emulator and the cross linkers. On Ubuntu:

```sh
sudo apt install qemu-user gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
                 gcc-riscv64-linux-gnu libc6-dev-riscv64-cross
rustup target add aarch64-unknown-linux-gnu riscv64gc-unknown-linux-gnu aarch64-apple-darwin
```

CI runs the same matrix on Linux, plus `cargo xtask kernel --headless`. Apple Silicon
isn't covered by CI: on a Mac, run `cargo xtask test --target host`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
