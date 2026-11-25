# green-tea

[![CI](https://github.com/jfernand/green-tea/actions/workflows/ci.yml/badge.svg)](https://github.com/jfernand/green-tea/actions/workflows/ci.yml)

Green threads (user-space cooperative tasks) in Rust, built from scratch to learn how
context switching works on x86_64, aarch64 and riscv64.

Write-up: [Multitasking](https://jotafernand.casaroja.es/posts/019-multitasking/)

Inspired by https://dzania.github.io/green-threads-from-scratch/

With minimal help from R. Claude and R. Junie

## Layout

The workspace has two versions side by side, so you can compare them, and three bare-metal
kernels built on the same context switch. Allocators, device drivers and kernels are grouped
under `allocators/`, `drivers/` and `kernels/`, respectively:

| crate | what it shows |
|---|---|
| [`green-tea-v1`](green-tea-v1) | The first working version, x86_64 only. One global `MAIN` context, `static mut` state, and a `VecDeque` `run()` scheduler. Its git history goes through the bugs: a shadowed `static`, a missing `done` flag, and a stack misaligned for the SysV ABI. |
| [`green-tea-v2`](green-tea-v2) | The same ideas split into layers: `arch` (save/restore registers, prime a stack) → `Task` (an asymmetric coroutine that switches back to whoever resumed it) → `run`/`spawn` (a round-robin scheduler on a thread-local run queue). No `static mut`. Runs on x86_64, aarch64 (including Apple Silicon) and riscv64. |
| [`green-tea-arch`](green-tea-arch) | The `arch` layer on its own: `Context` and `swap_context` for each architecture. `no_std`, so v2 and the kernels share it. |
| [`green-tea-pool`](allocators/pool) | Pools of fixed-size blocks, two ways: a bitmap and an intrusive free list. `no_std`; the kernels' task stacks come from one. |
| [`green-tea-fit`](allocators/fit) | An allocator for blocks of any size: a free list searched first-fit, next-fit or best-fit, with boundary tags to merge freed neighbours. `no_std`. Also a binary buddy system and TLSF, which is the kernels' heap, and replays recorded traces of allocations against each policy, and measures the fragmentation. |
| [`green-tea-kmem`](allocators/kmem) | What a kernel allocates for itself with: page frames from a buddy system that keeps its records in a table, apart from the pages, and slab caches of constructed objects on top. `no_std`; the kernels' task records come from one. |
| [`green-tea-region`](allocators/region) | Regions: a bump allocator over chunks taken from the heap, freed all at once. `alloc` returns a reference that lives as long as the borrow of the region, so the borrow checker refuses a use after the region is dropped or reset, and a pointer from a long-lived region into a short-lived one. `no_std`. |
| [`green-tea-trace`](green-tea-trace) | A command for the host that reads a trace file (`green-tea-fit`'s `tracefile` module: a text form and a binary form) and replays it against each allocator, as the shell's `replay` does in the kernel. `cargo xtask trace` gets such a file out of the kernel, over its serial console. |
| [`green-tea-measure`](green-tea-measure) | A command for the host that replays one trace file against every allocator here that one thread can use, and prints what each needed: bytes, free blocks searched, instructions (counted by valgrind's callgrind) and time as a distribution. Also the Lisp's programs with explicit frees placed by an oracle, against the same programs on its garbage-collected heaps. |
| [`green-tea-smp`](allocators/smp) | Allocating from several CPUs: a spin lock, and a cache of free blocks for each CPU in front of a heap behind the lock. `no_std`; its tests run on the host with real threads. |
| [`green-tea-lisp`](green-tea-lisp) | A small Lisp: a reader, an evaluator and a printer, over a heap it reaches only through the ten operations of a trait. The first heap counts references. `no_std`; `--kernel lisp` runs it. |
| [`green-tea-shard`](allocators/shard) | Allocating from several CPUs with no lock, after mimalloc: pages of one size class each, a free list in every page, and a second list in every page that other CPUs push to with a compare-and-swap. `no_std`; its tests run on the host with real threads. |
| [`green-tea-smr`](green-tea-smr) | Safe memory reclamation: a lock-free stack and queue with their nodes on the heap, and when a node they have unlinked may be freed, by epochs and by hazard pointers. `no_std`; its tests run on the host with real threads. |
| [`drivers/`](drivers) | The device drivers, one crate each, which know their device but not the machine: serial ports (16550, PL011), QEMU's fw_cfg and ramfb, drawing on a framebuffer, PCI through ECAM, virtio (MMIO and PCI transports, split virtqueues), virtio-blk, and for physical PCs Multiboot2, ACPI, a PS/2 keyboard, a USB keyboard over xHCI and an NVMe disk. |
| [`green-tea-bare`](green-tea-bare) | What the images share below their schedulers: boot code per board behind the `Platform` and `TimerInterrupt` traits (timers, interrupts, paging, and where each device is), plus a heap, task stacks, and platform services. |
| [`green-tea-scheduler-coop`](schedulers/coop) | Bare-metal task records, cooperative task operations, a round-robin runtime, and scheduler-aware console input. It depends on `green-tea-bare`; the bare-metal support crate does not depend on it. |
| [`green-tea-scheduler-preempt`](schedulers/preempt) | Timer-preemptive `run`, `spawn`, `sleep_ms`, and `yield_now` operations layered on the cooperative scheduler, with interrupt-safe task transitions and per-slice timer management. |
| [`green-tea-coop`](kernels/coop) | A bare-metal kernel for QEMU on riscv64, aarch64 and x86_64 whose tasks yield to each other, so you can watch the context switches on screen. |
| [`green-tea-preempt`](kernels/preempt) | The same, but the tasks never yield: a timer interrupt preempts them. One task sleeps instead, and costs no time slices. |
| [`green-tea-shell`](kernels/shell) | A kernel with a small shell on the serial console. Each command allocates its working memory from the heap, block by block, or from an arena that is reset after every command. |
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
cargo xtask kernel --kernel tlsf      # allocators that don't search: a buddy system and TLSF, against the ones that do
cargo xtask kernel --kernel slab      # page frames from a buddy system, and a slab cache for task records
cargo xtask kernel --kernel smp       # four CPUs: the heap behind a lock, then a cache for each CPU
cargo xtask kernel --kernel shard     # four CPUs and no lock: a heap of pages for each, against the caches
cargo xtask kernel --kernel lisp      # a small Lisp on a heap that counts references
cargo xtask kernel --kernel gc        # the same Lisp on heaps that mark and sweep, mark and compact, and copy
cargo xtask kernel --kernel gen       # the same Lisp on a heap with two generations, and what its write barrier records
cargo xtask kernel --kernel smr       # four CPUs on a lock-free stack and queue: when to free a node
cargo xtask kernel --kernel incremental  # the same Lisp on a heap that marks and sweeps in steps, paced by a timer
cargo xtask kernel --kernel shell     # the shell's test, typed at its serial console
cargo xtask kernel --kernel trace     # the shell records its heap, and replays the trace against each policy
cargo xtask kernel --kernel safety    # a task corrupts the free list; the allocator hardened, a defence at a time
cargo xtask kernel --kernel harden    # the recorded trace against the hardened allocator
cargo xtask kernel --kernel region    # the shell's script again, with its memory from two regions
cargo xtask kernel --kernel gtt       # the shell writes its recording as a trace file, and reads it back
cargo xtask trace                     # saves the shell's recording to target/kernel/shell-riscv64.gtt
cargo run --release -p green-tea-trace -- replay target/kernel/shell-riscv64.gtt   # replays it on the host
cargo run --release -p green-tea-measure -- space target/kernel/shell-riscv64.gtt   # and against every allocator
cargo xtask shell                     # the shell, on this terminal (--target to pick a board)
cargo xtask kernel --target aarch64   # just one architecture
cargo xtask kernel --headless --screenshot   # no windows; saves target/kernel/<kernel>-<arch>.png
```

### On a physical PC

The kernels build for QEMU by default. `--board pc` builds them for a physical x86_64 PC
booted by GRUB from UEFI, and tests them in QEMU with UEFI firmware:

```sh
sudo apt install grub-efi-amd64-bin ovmf
cargo xtask kernel --board pc --headless              # every kernel but disk
cargo xtask kernel --board pc --kernel shell --headless
```

To boot one on a machine, copy `target/kernel/pc/shell-x86_64/EFI` to a FAT-formatted USB
stick and choose the stick in the firmware's boot menu. Secure Boot must be off, since this
GRUB isn't signed. Output goes to the screen, and input comes from a PS/2 keyboard, which
is what most laptops' built-in keyboards are, or from a USB keyboard plugged straight into
the machine before it boots. USB hubs and USB disks aren't supported.

A build supports 8 CPUs unless told otherwise: `GREEN_TEA_MAX_CPUS=32 cargo xtask kernel ...`
(1 to 256). Each one costs 16 KiB of fault stack.

### Cooperative: green-tea-coop

The kernel runs with no OS, no interrupts and one CPU. It gives QEMU's ramfb device a
framebuffer by writing its address and shape to the fw_cfg file `etc/ramfb`. Then five
tasks each own a column: every time one is resumed, it grows its bar by one step and
yields. Before each resume, the scheduler outlines the column of the task about to run and
bumps the switch counter at the top. When all tasks finish, the status bar turns green and
the serial console prints the total, which should be 57; `cargo xtask kernel` checks it.

The scheduler is shared in [`schedulers/coop`](schedulers/coop), while
[`green-tea-bare/src/platform`](green-tea-bare/src/platform) supplies the machine-specific
parts; the drivers it picks are in [`drivers/`](drivers). Per architecture:

| | riscv64 (`virt`) | aarch64 (`virt`) | x86_64 (`q35`) |
|---|---|---|---|
| Boot | OpenSBI jumps to S-mode | QEMU jumps to EL1, MMU off | PVH: 32-bit mode, paging off; `_start` builds page tables and enters long mode |
| FPU | `sstatus.FS` on | `CPACR_EL1.FPEN` on | none (soft-float target) |
| Serial | 16550, memory-mapped | PL011 | 16550 on I/O ports |
| fw_cfg | memory-mapped | memory-mapped | I/O ports |
| virtio | MMIO, 8 slots from 0x1000_1000 | MMIO, 32 slots from 0x0a00_0000 | PCI, found through ECAM at 0xb000_0000 |
| Timer | `time` CSR | `cntvct_el0` | HPET |
| Timer interrupt | SBI `set_timer` → supervisor timer interrupt | virtual timer → GICv2, IRQ 27 | local APIC timer, one-shot, measured against the HPET |
| Paging | Sv39 identity map; guard pages unmapped, the heap's pages mapped on first touch | off | identity map (2 MiB pages; 4 KiB for the stack pool and the heap; guard pages unmapped, the heap's pages mapped on first touch) |

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
remember which slots are free. [`green-tea-pool`](allocators/pool) does that two ways behind
one trait. `Bitmap` keeps a bit per slot, apart from the slots, takes the lowest free one,
and notices a slot given back twice. `FreeList` threads a list through the free slots
themselves, takes the one most recently given back, and can check nothing: given a slot
twice, it hands that slot to two callers. The stack pool uses the free list, with each free
slot's link in the first word above its guard page.

`--kernel pool` gives both pools the same takes and gives and prints what each does, then
runs a thousand tasks one after another and checks that they all ran on the same stack.
The crate's own tests run on the host: `cargo test -p green-tea-pool`.

### A heap that frees

The kernels' heap is a [`green-tea-fit`](allocators/fit) allocator over the 4 MiB linker.ld
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
size. Then it runs an [adversary](allocators/fit/src/adversary.rs), a program that looks at
where its blocks are and frees whichever leave holes too small for what it asks for next.
Holding 64 KiB, it makes every policy need 5.93 times that. Last, it replays the `fit`
test's random workload against each policy.

The heap can record what is asked of it (`heap::record`), as a
[trace](allocators/fit/src/trace.rs): an address and a size per allocation, an address per
free. `--kernel trace` has the shell record itself running a script of 1,970 commands
that set, read and unset variables, and then replay the trace against all six
combinations of policy and list order. It replays it twice more: with the same sizes and
lifetimes dealt out at random, and with the long-lived blocks in a region of their own.

### Allocators that don't search

First fit by address, which the heap used to be, walks a list: with 8,000 holes in the way
it looks at 8,001 free blocks to allocate and passes 8,000 to free, with interrupts off.
green-tea-fit has two allocators that keep a list for each size instead.
[`Buddy`](allocators/fit/src/buddy.rs) allows only powers of two, splits a block only in
half, and merges a freed block only with the half it was split from, which it finds with
an exclusive or. [`Tlsf`](allocators/fit/src/tlsf.rs), two-level segregated fit, keeps
`Fit`'s blocks and boundary tags and replaces the one free list by a table of lists, one
for each size below 256 bytes and sixteen for each power of two above, with two bitmaps
that say which lists have a block. An allocation is two masks and two counts of trailing
zeros. The kernels' heap is now a `Tlsf`.

`--kernel tlsf` shows a buddy system splitting and merging, the block each allocator
makes for a request, which TLSF lists hold a block as blocks come and go, and then
compares four allocators on the random workload and on a heap built to make a search as
long as it can be.

### Page frames, and a slab cache

Under the heap the kernel has two allocators of its own, from
[`green-tea-kmem`](allocators/kmem). The first hands out page frames: the 16 MiB of RAM
after the kernel's last section, in blocks of 1 to 1,024 pages, by a binary buddy system.
It keeps a 12-byte record per frame in a table (in the region's first 12 pages) and
nothing in the pages, so it never touches a free page, knows the order of every allocated
block, and refuses a block freed twice. The kernel doesn't ask the board how much RAM
there is: `cargo xtask kernel` gives QEMU 128 MiB, and the region ends well inside it.

The second is a slab cache, after Bonwick's. A slab is one page cut into objects of one
type, with the slab's record in its last 40 bytes. Objects are constructed when the slab
is made and stay constructed while they are free, with the free list's link in a word
after each object. A task's record now comes from such a cache, and what its constructor
does is take a stack from the pool: a thousand tasks run one after another take 8 stacks
on riscv64 (one slab's worth of records) where they used to take a thousand. The cost is
that a slab of records holds a stack for each, used or not, so the most tasks alive at
once falls from 102 to 96 on riscv64, 100 on aarch64 and 92 on x86_64. When the scheduler
has nothing left to run it reaps the cache, and the stacks and the pages go back.

`--kernel slab` prints the free blocks by order as blocks are split and merged, frees
every other page to leave half the memory free with no two pages together, runs a random
workload, and then shows a cache of 200-byte objects: when objects are constructed, what
a freed slab keeps, and that ten objects can hold ten pages or one. The crate's own tests
run on the host: `cargo test -p green-tea-kmem`.

### A hardened heap

[`allocators/fit/src/hardened.rs`](allocators/fit/src/hardened.rs) is the first-fit allocator
again, with five defences that can each be switched on: safe unlinking (a block's
neighbours on the free list must point back at it before it is taken off), links stored
XORed with a secret, a canary word after the caller's bytes, a quarantine that holds freed
blocks filled with 0xdd and checks the fill as they leave, and random choices of free block
and quarantine slot from a seeded generator. `Hardened<D>` takes the defences as a const
parameter, so each combination is compiled on its own and its cost shows in the
disassembly. With `D` 0 it places blocks exactly where `Fit` does. The plain allocators
are unchanged.

`--kernel safety` first runs two tasks over a plain `Fit`: one writes to a block after
freeing it, and the other is handed memory inside a block that is still allocated. Then it
makes six mistakes against the allocator with one defence more each time and prints which
call caught each, counts how long a freed block waits before it is reused and where the
next blocks go, and replays the `fit` test's workload for the cost. `--kernel harden` has
the shell record itself as `trace` does, and replays that recording the same way.

### The heap's pages, mapped on demand
On riscv64 and x86_64 the page tables start with every page of the heap missing. The
first access to a page faults, the fault handler maps it
([pages.rs](green-tea-bare/src/pages.rs)), and the instruction runs again. The map is
still the identity. `pages::release` zeroes a page and unmaps it, and `pages::trim` does
that for the pages of the heap's free blocks that hold no header, link or footer
([allocators/fit's pages.rs](allocators/fit/src/pages.rs) says which those are). aarch64
runs with the MMU off, and has every page all the time.
```sh
cargo xtask kernel --kernel pages      # riscv64, x86_64
cargo xtask kernel --kernel resident   # riscv64, x86_64
```
`--kernel pages` allocates a megabyte, writes to part of it, then all of it, frees it and
trims, printing after each step which of the heap's 1,024 pages are mapped. It walks the
page table for six addresses and prints the entry at each level. Then it allocates a
megabyte of 48-byte blocks and frees all but one in 64, one in 128, and so on: one block
to a page keeps every page. `--kernel resident` has the shell record its script, as
`--kernel trace` does, and `pages replay` replays the recording against each allocator in
a region with no page mapped, counting the pages each one touches and the pages left
after trimming.

### Several CPUs

Every kernel boots on one CPU, and all but three tests stay there. `--kernel smp` and
`--kernel smr` are run on a machine with four (`-smp 4`), and start the other three
([`green-tea-bare/src/cpus.rs`](green-tea-bare/src/cpus.rs)). How is the board's business:

| | riscv64 (`virt`) | aarch64 (`virt`) | x86_64 (`q35`) |
|---|---|---|---|
| Start a CPU | SBI HSM `hart_start` | PSCI `CPU_ON`, by `hvc` | INIT and STARTUP from the local APIC, to a real-mode trampoline copied to 0x8000 |
| Which CPU am I | `tp`, set when the hart starts | `MPIDR_EL1` | the local APIC's id |
| What a started CPU sets up | FPU, `stvec`, `satp` | FP/SIMD, `VBAR_EL1` | long mode on the boot CPU's page tables, then its GDT, TSS and IDT |

A started CPU gets a stack of 64 KiB from the heap and waits to be handed a function;
`cpus::run` gives the same one to every CPU and returns when all are done. That is all the
multiprocessing there is. The scheduler, tasks, the console and every `Global` still belong
to the boot CPU, and a started CPU runs with interrupts off. What is safe from every CPU is
the heap: each operation takes a spin lock as well as turning interrupts off.

[`green-tea-smp`](allocators/smp) has the lock, and a `Cache`: one CPU's free blocks, in
twelve size classes, filled from and emptied into a `Central` (a heap, and a depot of
spare batches) eight blocks at a time, so that the lock is taken once per batch. The test
has four CPUs add to a counter with and without a lock, then make 80,128 allocations and
frees from the kernel's heap, which takes its lock 80,128 times, and then the same
through a cache each, which takes the lock behind them 2,680 times. The crate's own tests
run on the host, with threads: `cargo test -p green-tea-smp`.
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

[`green-tea-shard`](allocators/shard) does without the lock. An `Arena` is a region cut
into pages of 16 KiB, and each CPU has a `Heap` that owns some of them. A page holds
blocks of one size class and has three free lists in its header: one the owner allocates
from, one for the blocks the owner frees, and one that other CPUs push to with a
compare-and-swap, which the owner takes whole, with one swap, when its own list runs out.
`free` takes an address and nothing else. `--kernel shard` runs the smp test's work
through both: the caches take their lock 2,680 times and the heaps of pages take none,
and then every block is freed by a CPU that didn't allocate it, 38,400 of them, and each
goes back to its page. `cargo test -p green-tea-shard`.
### Heaps that trace

Three more heaps implement the same `Heap` trait, and the evaluator doesn't change.
[`MarkHeap`](green-tea-lisp/src/mark.rs) marks every object the root stack reaches, with a
mark bit in each header and a mark stack of a fixed size, and then either sweeps the heap
into free lists (`Reclaim::Sweep`) or slides the marked objects down to its start
(`Reclaim::Compact`). [`Semispace`](green-tea-lisp/src/semispace.rs) copies what the root
stack reaches into the other half of its memory with Cheney's scan, and allocates by
moving a pointer. They collect when `alloc` finds no room, and
[`gc.rs`](green-tea-lisp/src/gc.rs) has what they share: the `Collect` trait (`collect`,
the counts, a switch that collects at every allocation, a clock for timing) and
`reachable`.

`--kernel gc` types the `lisp` test's definitions at a heap that marks, runs the two
programs that leak under reference counting in 1,024 words, runs the five programs on
each heap and counts the collections and the objects traced, walked and moved, and shows
where objects are after a collection, what the mark stack holds, and what a reference
that the heap wasn't told about refers to afterwards. The smallest heap each program runs
in is found on the host: `cargo run --release -p green-tea-lisp --example heaps`.

[`green-tea-smr`](green-tea-smr) has a stack and a queue that take no lock: they change
their lists with compare-and-swap, and keep their nodes on the heap. A thread may be
reading a node at the moment another unlinks it, so the thread that unlinks a node doesn't
free it but *retires* it, to a `Reclaim` that frees it when nobody can still hold its
address. There are four: `AtOnce`, which is wrong; `Leak`, which never frees; `epoch`,
which frees a node once every thread has begun a new operation twice over (the form of
epoch-based reclamation that Trevor Brown calls DEBRA); and `hazard`, Maged Michael's
hazard pointers, which frees a node when no thread has announced that it is using it.
`--kernel smr` first interrupts a pop, on one CPU, with what another thread could do at
its worst moment: with `AtOnce` the stack ends up sharing a node with another stack, and
with the other two it doesn't. Then four CPUs share a stack and a queue; one of them
stops in the middle of an operation, which with epochs keeps every node retired
afterwards from being freed and with hazard pointers keeps one; one replaces a value by
copying it while the others read it (`rcu::Rcu`, the shape of read-copy update); and a
queue of jobs that split themselves in two is run by all four. The crate's own tests run on the host, with
threads: `cargo test -p green-tea-smr`.
### A heap with two generations

[`GenHeap`](green-tea-lisp/src/generational.rs) is a fifth heap behind the same trait. Every
object is made in a nursery at the top of the heap's words, by moving a pointer. A minor
collection copies what survives to the end of an old space at the bottom, and traces
nothing there: it starts from the root stack and from a remembered set, which the write
barrier in `set_field` fills with the old objects that were made to refer to nursery
objects (`Barrier::Filter`), or with every object stored into (`Barrier::Every`). The
nursery is at most half of the unused words, so the survivors always fit, and the old
space is compacted in place when it passes half of the heap.

`--kernel gen` types a few lines at it and prints where one pair is after each, runs seven
programs (the five, and two that store into old objects) and counts what was copied out of
the nursery and what the barrier recorded, and shows a minor collection that traces
nothing with 10,000 objects in the old space, and what a store without the barrier leaves
behind. The longer tables are made on the host: `cargo run --release -p green-tea-lisp
--example generations`, and `--example store_cost` under `valgrind --tool=callgrind` for
what a store costs in instructions.
### A heap that marks in steps

[`IncHeap`](green-tea-lisp/src/incremental.rs) is `MarkHeap`'s mark and sweep done a few
objects at a time inside `alloc`, with the program running in between. `set_field` has a
write barrier (`Barrier::Insertion` shades the object stored, `Barrier::Deletion` the one
overwritten, `Barrier::None` frees objects in use), objects allocated during marking are
born white or black (`Birth`), and the steps are paced by allocation, by a tick that
`alloc` polls, or not at all (`Pace`). `--kernel incremental` shows an object freed in use
without a barrier, compares the longest step with a whole cycle, and paces the steps with
a timer interrupt. The tables are made on the host:
`cargo run --release -p green-tea-lisp --example incremental`.

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

### Regions

[`green-tea-region`](allocators/region) is the arena again with its lifetimes in the types.
A `Region` takes chunks from the heap as it fills, and gives them all back when it is
dropped; `reset` keeps one chunk to fill again. `region.alloc(value)` returns `&mut T`
borrowed from the region, `Vec::new_in(&region)` grows in it, and `scope` is a region that
lasts for one call. A region runs no destructors, so `alloc` refuses, at build time, a type
that has one. The crate's documentation has five programs the compiler refuses, as
`compile_fail` tests: `cargo test -p green-tea-region`.

`--kernel region` types `regions` at the shell, which runs the session's script of 1,970
commands four ways with the heap recording: everything on the heap (10,316 allocations),
commands in the arena (2,996), commands in a region that is reset and variables in a
region for the session (16), and the same with a new region for every command (1,985).
It also prints what the session's region held at the end: 57,693 bytes, of which the
variables still set are 6,609. A region frees nothing until it frees everything.

`cargo run --release -p green-tea-region --example cost -- region 100000` runs the shell's
`echo` on the host with its memory from `malloc`, first fit, TLSF or a region, for counting
instructions under valgrind.

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
