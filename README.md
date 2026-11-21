# green-tea

[![CI](https://github.com/jfernand/green-tea/actions/workflows/ci.yml/badge.svg)](https://github.com/jfernand/green-tea/actions/workflows/ci.yml)

Green threads (user-space cooperative tasks) in Rust, built from scratch to learn how
context switching works on x86_64, aarch64 and riscv64.

Write-up: [Multitasking](https://jotafernand.casaroja.es/posts/019-multitasking/)

Inspired by https://dzania.github.io/green-threads-from-scratch/

With minimal help from R. Claude and R. Junie

## Layout

The workspace has two versions side by side, so you can compare them, and three bare-metal
kernels built on the same context switch:

| crate | what it shows |
|---|---|
| [`green-tea-v1`](green-tea-v1) | The first working version, x86_64 only. One global `MAIN` context, `static mut` state, and a `VecDeque` `run()` scheduler. Its git history goes through the bugs: a shadowed `static`, a missing `done` flag, and a stack misaligned for the SysV ABI. |
| [`green-tea-v2`](green-tea-v2) | The same ideas split into layers: `arch` (save/restore registers, prime a stack) → `Task` (an asymmetric coroutine that switches back to whoever resumed it) → `run`/`spawn` (a round-robin scheduler on a thread-local run queue). No `static mut`. Runs on x86_64, aarch64 (including Apple Silicon) and riscv64. |
| [`green-tea-arch`](green-tea-arch) | The `arch` layer on its own: `Context` and `swap_context` for each architecture. `no_std`, so v2 and the kernels share it. |
| [`green-tea-pool`](green-tea-pool) | Pools of fixed-size blocks, two ways: a bitmap and an intrusive free list. `no_std`; the kernels' task stacks come from one. |
| [`green-tea-fit`](green-tea-fit) | An allocator for blocks of any size: a free list searched first-fit, next-fit or best-fit, with boundary tags to merge freed neighbours. `no_std`; the kernels' heap is one. Also replays recorded traces of allocations against each policy, and measures the fragmentation. |
| [`green-tea-lisp`](green-tea-lisp) | A small Lisp: a reader, an evaluator and a printer, over a heap it reaches only through the ten operations of a trait. The first heap counts references. `no_std`; `--kernel lisp` runs it. |
| [`drivers/`](drivers) | The device drivers, one crate each, which know their device but not the machine: serial ports (16550, PL011), QEMU's fw_cfg and ramfb, PCI through ECAM, virtio (MMIO and PCI transports, split virtqueues) and virtio-blk. |
| [`green-tea-bare`](green-tea-bare) | What the kernels share: boot code per board behind the `Platform` and `TimerInterrupt` traits (timers, interrupts, paging, and where each device is), a port of v2's `Task` and scheduler, and a heap. |
| [`green-tea-coop`](green-tea-coop) | A bare-metal kernel for QEMU on riscv64, aarch64 and x86_64 whose tasks yield to each other, so you can watch the context switches on screen. |
| [`green-tea-preempt`](green-tea-preempt) | The same, but the tasks never yield: a timer interrupt preempts them. One task sleeps instead, and costs no time slices. |
| [`green-tea-shell`](green-tea-shell) | A kernel with a small shell on the serial console. Each command allocates its working memory from the heap, block by block, or from an arena that is reset after every command. |
| [`xtask`](xtask) | `cargo xtask test` runs v2 across targets under qemu; `cargo xtask kernel` boots the kernels everywhere they run; `cargo xtask shell` boots the shell on your terminal. |

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
cargo xtask kernel --kernel overflow  # the guard page test (riscv64, x86_64)
cargo xtask kernel --kernel disk      # the virtio-blk test
cargo xtask kernel --kernel memmap    # where the linker put each section
cargo xtask kernel --kernel pool      # two pools compared, and a thousand tasks on one stack
cargo xtask kernel --kernel fit       # the heap's allocator: merging, three ways to search, a thousand tasks
cargo xtask kernel --kernel frag      # fragmentation: an adversary, and a random workload
cargo xtask kernel --kernel lisp      # a small Lisp on a heap that counts references
cargo xtask kernel --kernel shell     # the shell's test, typed at its serial console
cargo xtask kernel --kernel trace     # the shell records its heap, and replays the trace against each policy
cargo xtask shell                     # the shell, on this terminal (--target to pick a board)
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

Everything but [`green-tea-bare/src/platform`](green-tea-bare/src/platform) is shared;
the drivers it picks are in [`drivers/`](drivers). Per architecture:

| | riscv64 (`virt`) | aarch64 (`virt`) | x86_64 (`q35`) |
|---|---|---|---|
| Boot | OpenSBI jumps to S-mode | QEMU jumps to EL1, MMU off | PVH: 32-bit mode, paging off; `_start` builds page tables and enters long mode |
| FPU | `sstatus.FS` on | `CPACR_EL1.FPEN` on | none (soft-float target) |
| Serial | 16550, memory-mapped | PL011 | 16550 on I/O ports |
| fw_cfg | memory-mapped | memory-mapped | I/O ports |
| virtio | MMIO, 8 slots from 0x1000_1000 | MMIO, 32 slots from 0x0a00_0000 | PCI, found through ECAM at 0xb000_0000 |
| Timer | `time` CSR | `cntvct_el0` | HPET |
| Timer interrupt | SBI `set_timer` → supervisor timer interrupt | virtual timer → GICv2, IRQ 27 | local APIC timer, one-shot, measured against the HPET |
| Paging | Sv39 identity map, guard pages unmapped | off | identity map (2 MiB pages; 4 KiB for the stack pool, guard pages unmapped) |

Differences from v2: `thread_local!` becomes a plain static (there's one CPU), and stacks
come from a pool in which each 16 KiB stack has a 4 KiB guard page under it. riscv64 and
x86_64 map memory with the guard pages missing, so there an overflow faults (aarch64 runs
with the MMU off); the
`overflow` test checks that a task that recurses forever faults in its own guard page,
like v2's `overflow_hits_the_guard_page`. A fault is handled on a stack of its own (on x86_64, IST1
in the TSS), since the task's is the one that's full.

### Preemptive: green-tea-preempt

Same screen, but the workers grow their bars in a busy loop and never call `yield_now`.
Before resuming a task, the scheduler sets a timer deadline 100 ms ahead. When it passes,
the CPU traps into a handler that saves *every* register on the task's stack, then calls
the same `yield_now` a cooperative task would. When the task is next resumed,
`swap_context` returns into the handler, which restores the registers and returns from the
trap (`sret`, `eret`, `iretq`) back into the loop. Each band of shading in a bar is one time slice.

The fifth worker sleeps between steps instead (`sleep_ms`). A sleeping task is parked off
the run queue until its deadline, so it costs no time slices: it uses about 1% of the CPU
of the busy workers, and its bar gets a thin stripe per wakeup. When it's the only task
left, the scheduler idles until it's due (an empty box in the status bar).

`swap_context` itself is unchanged: it only saves callee-saved registers because, to the
compiler, it's a function call. An interrupt isn't, so the trap frame covers the rest.
The other rule is that the scheduler runs with interrupts off, and tasks with them on.

The kernel needs a board that implements `TimerInterrupt`; all three do. A board that
didn't would fail to build on that trait bound.

### A disk: virtio-blk

`--kernel disk` runs green-tea-coop with a disk attached. xtask writes a fresh 32 KiB
image in which every byte depends on its sector and offset. The kernel reads it all back,
four sectors per request, then overwrites one sector, flushes, and checks that sector and
its neighbours.

virtio splits a device in two. The device model is the same everywhere: a status
handshake, feature bits, a configuration area (the disk's capacity), and virtqueues,
rings in guest memory through which the driver hands the device chains of buffers. A disk
read is a chain of three: a header the device reads, the data it writes, and a status
byte. The transport, how the driver reaches the device's registers, is what differs:

- on riscv64 and aarch64 `virt`, a block of registers in one of a row of fixed slots
  (virtio-mmio). QEMU makes those legacy devices unless started with
  `-global virtio-mmio.force-legacy=false`;
- on x86_64 `q35`, a PCI device. The firmware has given its BARs addresses, and
  vendor-specific PCI capabilities say where in them each register block is.

The block driver is written once, against the `Transport` trait, and polls for
completions: no interrupts yet.

### The memory map

`--kernel memmap` runs green-tea-coop's test of its own layout. linker.ld defines a symbol
at the start and end of each section, and the kernel reads their addresses
([`green-tea-bare/src/memory.rs`](green-tea-bare/src/memory.rs)), prints the map, and
checks it: every section starts on its alignment, they are in order, and the boot stack,
heap and stack pool have the sizes linker.ld reserves. It also checks one address of each
kind: a function is in `.text`, a constant in `.rodata`, statics in `.data` and `.bss`, a
local of `kmain`'s on the boot stack, a `Box` in the heap, and a task's local in the stack
pool.

The gaps it prints between sections are sections linker.ld doesn't name, which the linker
places itself: `.eh_frame`, and on x86_64 `.got` and the PVH note.

### Pools

Task stacks are all one size, so the pool they come from needs no search: it only has to
remember which slots are free. [`green-tea-pool`](green-tea-pool) does that two ways behind
one trait. `Bitmap` keeps a bit per slot, apart from the slots, takes the lowest free one,
and notices a slot given back twice. `FreeList` threads a list through the free slots
themselves, takes the one most recently given back, and can check nothing: given a slot
twice, it hands that slot to two callers. The stack pool uses the free list, with each free
slot's link in the first word above its guard page.

`--kernel pool` gives both pools the same takes and gives and prints what each does, then
runs a thousand tasks one after another and checks that they all ran on the same stack.
The crate's own tests run on the host: `cargo test -p green-tea-pool`.

### A heap that frees

The kernels' heap is a [`green-tea-fit`](green-tea-fit) allocator over the 4 MiB linker.ld
reserves. Every block has a word at each end, a header and a footer, holding its size and
whether it is allocated (Knuth's boundary tags), and the free blocks are on a doubly linked
list threaded through themselves. `allocate` searches the list and splits the block it
finds; `free` looks one word below the block and one word above it, and merges with
whichever neighbours are free. The crate has three searches (first fit, next fit, best fit)
and two list orders (LIFO, by address); the heap uses first fit by address. It runs with
interrupts off, since a free list can't be updated in one instruction.

`--kernel fit` prints a small region word by word, frees blocks until they merge back into
one, gives one request to each policy, makes the caller's mistakes (a second free and a
write past the end are refused; a write to a freed block redirects the list), counts what
each policy does over the same 20,000 operations, and checks that a thousand tasks leave
the kernel's heap as they found it. The crate's own tests run on the host:
`cargo test -p green-tea-fit`.

### Fragmentation, measured

green-tea-fit keeps a *footprint*: the highest address any allocated block has reached. A
region that size would have been enough for everything asked of the allocator so far, so
the footprint against what the program had allocated at its peak is how much the
allocator's placement cost.

`--kernel frag` first shows a region that is half free and refuses a request a fifth its
size. Then it runs an [adversary](green-tea-fit/src/adversary.rs), a program that looks at
where its blocks are and frees whichever leave holes too small for what it asks for next.
Holding 64 KiB, it makes every policy need 5.93 times that. Last, it replays the `fit`
test's random workload against each policy.

The heap can record what is asked of it (`heap::record`), as a
[trace](green-tea-fit/src/trace.rs): an address and a size per allocation, an address per
free. `--kernel trace` has the shell record itself running a script of 1,970 commands
that set, read and unset variables, and then replay the trace against all six
combinations of policy and list order. It replays it twice more: with the same sizes and
lifetimes dealt out at random, and with the long-lived blocks in a region of their own.

### A Lisp, and a heap that counts references

[`green-tea-lisp`](green-tea-lisp) is an interpreter for a small Lisp: integers, symbols,
pairs, `quote`, `if`, `define`, `lambda` with lexical closures and tail calls, and thirteen
primitives. Its objects live in a heap that the interpreter reaches only through the
`Heap` trait: a root stack (`push`, `pop`, `root`, `set_root`), `alloc`, which takes an
object's fields from the root stack, and `field` and `set_field`. The interpreter holds
indices, never addresses, and keeps everything it needs across an allocation on the root
stack, so a heap that traces or moves objects can replace the first one, `RcHeap`, which
counts references. The top of [`green-tea-lisp/src/lib.rs`](green-tea-lisp/src/lib.rs) has
the interface and its one rule.

`RcHeap` keeps a count in each object's header and frees an object when its count reaches
zero: by recursion, from a queue threaded through the dead objects, or lazily, a few at
each later allocation. It can't free a cycle, unless it is built with the cycle collector
(trial deletion). The programs it is measured with are in
[`green-tea-lisp/programs`](green-tea-lisp/programs).

`--kernel lisp` runs the five programs and prints what each allocated and freed and how
many times a count was raised or lowered, frees a list each of the three ways, and runs
the two programs that make cycles in a heap too small for what they leak. Every number is
a count, the same on each architecture, but for the bytes of stack the recursion used. The
crate's own tests run on the host: `cargo test -p green-tea-lisp`.

### A shell, and an arena

green-tea-shell is one task that reads a line from the serial console and runs it as a
command, forever. The serial ports are polled: a task waiting for input sleeps a
millisecond at a time. Type at it with `cargo xtask shell`; `help` lists the commands.

Every command allocates its working memory (a copy of the line, the words it splits into,
the text it prints) and is done with it when it returns. `use heap` takes that memory from
the global heap: eight allocations for an `echo` of nine words, each found by a search and
freed on its own. `use arena` takes it from an [arena](green-tea-bare/src/arena.rs), a bump
allocator over a block of its own that is reset after every command. `heap` prints the
heap block by block. `set`, `unset` and `vars` keep variables, which are always on the heap,
since they outlive the command that sets them. The arena is an `Allocator`,
and `reset` takes `&mut self`, so the borrow checker refuses a reset while anything
allocated from the arena is still alive.

`--kernel shell` types two commands at the shell, through a file that QEMU feeds to the
serial port. The second is `test`, which runs 1,000 commands each way and checks that
commands run from the heap free everything they allocate, and that commands run from the
arena don't touch the heap. (The script starts with an empty line,
because on riscv64 the first byte typed before the kernel is up doesn't arrive.)

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
