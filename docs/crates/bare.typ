#import "../lib.typ": *

= green-tea-bare <crate-bare>

`green-tea-bare` is what every kernel in the workspace stands on. It boots four boards, three of them QEMU machines (`virt` on riscv64 and aarch64, `q35` on x86_64) and the fourth a PC booted by GRUB (`pc`, on x86_64, chosen at build time), hides each behind one trait, and provides a heap, page frames, task stacks, a console, a screen, cooperative tasks, a round-robin scheduler, and a way to start the other CPUs. A kernel is a `#![no_std] #![no_main]` binary that defines `kmain` and calls in.

#spec(
  ("Source", [#repo("green-tea-bare/src/"), 20 files, 4,226 lines; #repo("green-tea-bare/linker.ld"), #repo("green-tea-bare/build.rs")]),
  ("Depends on", [`green-tea-arch`, `-pool`, `-fit`, `-kmem`, `-smp`, and the drivers `io`, `fw-cfg`, `framebuffer`, `ramfb`, `pci`, `virtio`, `virtio-blk`, with `ns16550` (riscv64, x86_64) or `pl011` (aarch64), and on x86_64 `acpi`, `i8042`, `multiboot2` and `xhci`]),
  ("Used by", [`green-tea-coop`, `green-tea-preempt`, `green-tea-shell`]),
  ("Tests", [none of its own (`test = false`): it builds only for the three bare-metal targets. The kernels of @crate-coop, @crate-preempt and @crate-shell exercise it]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [crate root], [`boot`, `delay_ms`, the panic handler, and re-exports: `Board`, `Platform`, `TimerInterrupt`, `VirtioBus`, `Qemu` (not on `pc`), `Framebuffer`, `Global`, `fit`, `fw_cfg`, `pci`, `pool`, `ramfb`, `smp`, `virtio`, `virtio_blk`],
  [`platform`], [the traits `Platform`, `VirtioBus` and `TimerInterrupt`; `qemu`, with the trait `Qemu`; `aarch64`, `riscv64`, and `x86_64` with its boards `q35` and `pc`],
  [`memory`], [`Section`, `sections`, `section_of`, `kernel`],
  [`heap`], [the global allocator, `NAME`, `align_up`, `Recording`, `record`, `stop_recording`, `is_recording`, `with`, `used`, `capacity`, `lock_counts`],
  [`pages`], [demand paging: `PAGE`, `Stats`, `Step`, `PointsTo`, `walk`, `heap_range`, `stats`, `fault`, `map_all`, `is_mapped`, `mapped_in`, `for_each_run`, `release`, `trim`],
  [`kmem`], [page frames and slab caches: `REGION`, `PAGES`, `TABLE_PAGES`, `region`, `with_frames`, `KernelCache`],
  [`stack`], [the stack pool: `PAGE`, `STACK_SIZE`, `SLOT`, `REGION`, `SLOTS`, `Stack`, `region_start`, `free_slots`, `guard_pages`, `guard_slot`, `on_overflow`, `overflowed`],
  [`arena`], [`Arena`, a bump allocator that can be reset],
  [`task`], [`Task`, `yield_now`, `sleep_until`, `sleep_ms`, `in_task`, `current_guard`, `cache`, `reap`],
  [`runtime`], [`run`, `spawn`, `Switch`],
  [`cpus`], [`MAX`, `count`, `start_all`, `run`, `park`, `enter`],
  [`console`], [`Console`, `read_byte`, `print!`, `println!`],
  [`screen`], [`Screen`, `Painter`, `COLUMNS`, `COLORS`, `BAR_MAX`, `BACKGROUND`],
)

== Boot

`boot` is the first call of every `kmain`. It initialises the board, prints the banner that `cargo xtask kernel` waits for, and asks the board for its display:

#srcfile("green-tea-bare/src/lib.rs", lines: ((67, 74),), notes: ((68, [Per board: paging on riscv64 and x86_64, the UART and the exception vectors on aarch64.]), (70, [On QEMU's boards, `ramfb` through `fw_cfg`. On `pc`, the framebuffer GRUB set up. A QEMU board with no display it can use panics on the next line but one. `pc` never returns an error here.]),))[```rust
pub fn boot(kernel: &str) -> Framebuffer {
    Board::init();
    println!("{kernel} on {}", Board::NAME);
    match Board::display() {
        Ok(fb) => fb,
        Err(e) => panic!("{e}"),
    }
}
```]

`delay_ms` busy-waits on `Board::ticks`. The panic handler prints the message and calls `Board::halt`, so a panic stops the CPU that took it and nothing else.

=== riscv64

QEMU runs OpenSBI (`-bios default`), which enters `_start` in S-mode at `0x80200000` with the hart id in `a0`. The entry sets the stack, zeroes `.bss`, turns the FPU on, and records which hart it is on, because OpenSBI doesn't always boot on hart 0. CPU 0 is whichever hart this is, and every hart keeps its CPU number in `tp`:

#srcfile("green-tea-bare/src/platform/riscv64.rs", lines: ((35, 59),), notes: ((49, [`sstatus.FS`. `swap_context` saves `fs0`–`fs11`, and with the FPU off the first `fsd` is an illegal instruction.]), (53, [`tp`, which compiled code leaves alone on this target, is the CPU number that `cpu()` reads.]),))[```rust
_start:
    la sp, __stack_top

    # Zero .bss (it holds the framebuffer and the statics).
    la t0, __bss_start
    la t1, __bss_end
1:  bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b
2:
    # Make sure the FPU is on (sstatus.FS != Off). swap_context saves fs0-fs11, and with
    # FS = Off its first fsd traps as an illegal instruction. OpenSBI happens to hand over
    # with the FPU on, but the SBI spec doesn't promise it, so don't rely on that.
    li t0, 1 << 13
    csrs sstatus, t0

    # This hart is CPU 0. Remember which hart it is: the others are started by hart id.
    mv tp, zero
    la t0, green_tea_boot_hart
    sd a0, 0(t0)

    call kmain
3:  wfi
    j 3b
```]

=== aarch64

There is no firmware. QEMU loads the ELF and starts `_start` at EL1 with the MMU off, which makes every data access a Device access, where an unaligned access faults. The `aarch64-unknown-none` target compiles with `+strict-align`, so the compiler makes none:

#srcfile("green-tea-bare/src/platform/aarch64.rs", lines: ((30, 53),), notes: ((33, [`CPACR_EL1.FPEN`: the target uses NEON, and without this the first such instruction traps.]),))[```rust
_start:
    // Enable FP/SIMD (CPACR_EL1.FPEN = 0b11) before any Rust runs: the target uses NEON,
    // and swap_context saves d8-d15. Otherwise the first such instruction traps.
    mrs x1, cpacr_el1
    orr x1, x1, #(3 << 20)
    msr cpacr_el1, x1
    isb

    adrp x1, __stack_top
    add x1, x1, :lo12:__stack_top
    mov sp, x1

    // Zero .bss (it holds the framebuffer and the statics).
    adrp x1, __bss_start
    add x1, x1, :lo12:__bss_start
    adrp x2, __bss_end
    add x2, x2, :lo12:__bss_end
1:  cmp x1, x2
    b.hs 2f
    str xzr, [x1], #8
    b 1b
2:  bl kmain
3:  wfi
    b 3b
```]

=== x86_64

QEMU boots the ELF through *PVH*: it finds a 32-bit entry point in a Xen ELF note and jumps there in protected mode with paging off. That is how `q35` boots; the `pc` board enters the same `_start` from GRUB (@bare-pc). The note is six words of assembly:

#srcfile("green-tea-bare/src/platform/x86_64/q35.rs", lines: ((21, 29),))[```rust
.section .note.Xen, "a", @note
.balign 4
.long 4
.long 4
.long 18
.asciz "Xen"
.balign 4
.long _start
.balign 4
```]

`_start`, in #repo("green-tea-bare/src/platform/x86_64/mod.rs"), is shared by both boards. It keeps what a Multiboot2 loader left in `eax` and `ebx`, then does what a bootloader would. It zeroes `.bss`, builds an identity map of the first 4 GiB out of 2,048 pages of 2 MiB, and enters long mode:

#srcfile("green-tea-bare/src/platform/x86_64/mod.rs", lines: ((84, 117),), notes: ((84, [`0x83`: present, writable, page size. No cache-disable bit: on `pc`, `map_device` adds it for device registers.]), (102, [With `EFER.LME` set, turning paging on is what enters long mode.]), (105, [The far jump reloads `CS` with a 64-bit code segment.]),))[```rust
    mov $0x83, %eax
    xor %ecx, %ecx
2:  mov %eax, boot_pds(,%ecx,8)
    add $0x200000, %eax
    inc %ecx
    cmp $2048, %ecx
    jne 2b

    mov %cr4, %eax
    or $(1 << 5), %eax          # PAE
    mov %eax, %cr4
    mov $boot_pml4, %eax
    mov %eax, %cr3
    mov $0xC0000080, %ecx       # EFER
    rdmsr
    or $(1 << 8), %eax          # LME: long mode enable
    wrmsr
    mov %cr0, %eax
    or $0x80000001, %eax        # PG | PE: paging on, and with LME that means long mode
    mov %eax, %cr0
    lgdt boot_gdt_ptr
    ljmp $0x08, $boot_start64   # reload CS with the 64-bit code segment

    .code64
boot_start64:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    xor %ax, %ax
    mov %ax, %fs
    mov %ax, %gs
    mov $__stack_top, %rsp
    call kmain
```]

The x86_64 target is soft-float, so neither `swap_context` nor the interrupt entry has floating-point registers to save.

== The linker script and the memory map

One script, #repo("green-tea-bare/linker.ld"), lays out every kernel. `build.rs` fills in its one variable, the load address, which on x86_64 depends on the board, and hands the result to the kernels through Cargo's `links` metadata, since a library's build script can't add linker arguments to the binaries that depend on it. Each kernel's own `build.rs` passes `-T` with the path from `DEP_GREEN_TEA_BARE_LINKER_SCRIPT`. The same script checks `GREEN_TEA_MAX_CPUS` (@bare-smp).

#srcfile("green-tea-bare/build.rs", lines: ((24, 31),))[```rust
let base = match arch.as_str() {
    "riscv64" => "0x80200000", // where OpenSBI jumps, 2 MiB into RAM
    "aarch64" => "0x40200000", // 2 MiB into RAM, leaving QEMU room for the device tree
    // UEFI firmware keeps data of its own a few MiB up, where the kernel would overlap it.
    "x86_64" if pc => "0x2000000", // 32 MiB
    "x86_64" => "0x100000",    // 1 MiB, above the legacy BIOS area
    other => panic!("green-tea-bare has no platform for {other}"),
};
```]

After `.text`, `.rodata`, `.data` and `.bss` come three reserved regions:

#srcfile("green-tea-bare/linker.ld", lines: ((55, 75),))[```text
.stack (NOLOAD) : ALIGN(4096) {
    __stack_bottom = .;
    . += 64K;
    __stack_top = .;
}

/* Memory for the heap (tasks, closures, the run queue): a TLSF allocator, see heap.rs. */
.heap (NOLOAD) : ALIGN(4096) {
    __heap_start = .;
    . += 4M;
    __heap_end = .;
}

/* Task stacks, each with a guard page under it (see src/stack.rs). One 2 MiB page,
 * so a board that pages can map just this region with 4 KiB pages. */
.stacks (NOLOAD) : ALIGN(2M) {
    __stacks_start = .;
    . += 2M;
    __stacks_end = .;
}
__kernel_end = .;
```]

#layers(caption: [The kernel's memory, highest address first],
  ([`.stacks`], [2 MiB, aligned to 2 MiB: task stacks, each over a guard page], "amber"),
  ([`.heap`], [4 MiB: the TLSF heap, 1,024 pages], "dark"),
  ([`.stack`], [64 KiB: the boot stack, for `kmain` and the scheduler], "dark"),
  ([`.bss`], [statics, the framebuffer, the page tables], "light"),
  ([`.data`, `.rodata`], [initialised and constant statics], "light"),
  ([`.text`], [`_start` first], "light"),
)

Above `__kernel_end` the `kmem` module takes another 16 MiB of RAM for page frames. `memory::sections` returns the seven sections as `Section` records, each with `name`, `start`, `end`, `align` and `reserved`, and the methods `size`, `range` and `contains`; `section_of` finds the one an address is in, and `kernel` is the whole range.

Two linker symbols can have the same address, such as `__stacks_end` and `__kernel_end`. Rust and LLVM assume two distinct statics never do, so an optimised build folds `stacks_end == kernel_end` to false without looking. Every address therefore passes through `black_box`:

#srcfile("green-tea-bare/src/memory.rs", lines: ((72, 74),))[```rust
fn address(symbol: *const u8) -> usize {
    black_box(symbol as usize)
}
```]

The `memmap` kernel prints the result and checks it from inside. On riscv64:

#output(title: "cargo xtask kernel --kernel memmap --target riscv64")[
```
kernel: 0x80200000..0x80c00000, 10240 KiB
  section      start        end      bytes    align
  .text    0x80200000 0x802eefbc    978876        2
  .rodata  0x802eefc0 0x8031ac6b    179371       16
  .data    0x8031bd70 0x8031be70       256       16
  .bss     0x8031c000 0x80472000   1400832       16
  .stack   0x80472000 0x80482000     65536     4096
  .heap    0x80482000 0x80882000   4194304     4096
  (gap)                            1564672
  .stacks  0x80a00000 0x80c00000   2097152  2097152
PASS: 7 sections in order and aligned; 8 addresses where they should be
```
]

The 1,564,672-byte gap is the alignment of `.stacks` to 2 MiB. The load addresses differ by board, and so do the sizes:

#output(title: "cargo xtask kernel --kernel memmap --target aarch64, then x86_64")[
```
kernel: 0x40200000..0x40c00000, 10240 KiB
  .heap    0x404d4000 0x408d4000   4194304     4096
  .stacks  0x40a00000 0x40c00000   2097152  2097152
kernel: 0x100000..0xc00000, 11264 KiB
  .heap    0x00419000 0x00819000   4194304     4096
  .stacks  0x00a00000 0x00c00000   2097152  2097152
```
]

== The `Platform` trait

Each build targets one machine, so nothing here is dynamic. `Board` is a type alias chosen by `cfg`: `RiscvVirt`, `AarchVirt`, or on x86_64 `Q35`, unless the `board-pc` feature picks `Pc`. What the trait buys is a checked contract: a kernel that needs timer interrupts says `B: TimerInterrupt`, and a board without them would fail to compile with that message.

#dtable(columns: (auto, 1fr), ("Item", "What a board provides"),
  [`NAME`], [the architecture, for the banner],
  [`GUARD_PAGES`], [whether touching a guard page faults],
  [`DEMAND_PAGING`], [whether the heap's pages start unmapped (default `false`)],
  [`set_mapped`, `is_mapped`, `walk`], [the heap's page table entries, and a software walk (defaults: always mapped, no walk)],
  [`init`], [what the rest needs; called once, first],
  [`putc`, `getc`], [the serial console; `getc` is polled],
  [`ticks`, `ticks_per_ms`], [a free-running counter],
  [`halt`], [stop for good],
  [`cpu`, `start_cpu`], [this CPU's number, and starting another (defaults: CPU 0, and an error)],
  [`display`], [turn on the screen and return its `Framebuffer`, or an error],
  [`test_name`], [the test this boot should run; none means the kernel's demo],
  [`ram_end`], [the end of RAM, on a board where an access past it would go unnoticed (default `None`)],
)

`Platform` doesn't mention QEMU. What only some boards have is in two traits of its own, so that code needing them says so in its bounds. `VirtioBus`, with `for_each_virtio` and `find_virtio`, is implemented by the three QEMU boards, and the `disk` kernel requires it. `Qemu`, in the module `qemu`, is how those boards reach the device QEMU passes files through. It has an associated type `FwCfg`, a constant `FW_CFG` and a method `fw_cfg`, and provides `ramfb_display` and `fw_cfg_test_name`, which those boards' `display` and `test_name` call. The module isn't compiled on `pc`.

The three QEMU implementations, side by side:

#dtable(columns: (auto, 1fr, 1fr, 1fr), ("", "riscv64", "aarch64", "x86_64 (q35)"),
  [Load address], [`0x80200000`], [`0x40200000`], [`0x100000`],
  [Entered], [S-mode, from OpenSBI], [EL1, from QEMU], [32-bit, PVH],
  [Serial], [16550 at `0x1000_0000`], [PL011 at `0x0900_0000`], [16550 at port `0x3f8`],
  [`ticks`], [`time` CSR, taken as 10 MHz], [`cntvct_el0`], [HPET at `0xfed0_0000`],
  [Timer], [SBI `set_timer`], [virtual timer, PPI 11, GICv2], [local APIC, one-shot],
  [Paging], [Sv39], [none: MMU off], [4-level, from `_start`],
  [Guard pages], [yes], [no], [yes],
  [Demand paging], [yes], [no], [yes],
  [Other CPUs], [SBI HSM], [PSCI `CPU_ON`], [INIT and two STARTUPs],
  [CPU number], [`tp`], [`MPIDR_EL1` low byte], [local APIC id],
  [fw_cfg], [MMIO `0x1010_0000`], [MMIO `0x0902_0000`], [I/O ports],
  [virtio], [8 MMIO slots], [32 MMIO slots], [PCI, ECAM at `0xb000_0000`],
)

Every address in that table is written in the source, and these three boards read no device tree or ACPI table, so each runs on its QEMU machine and no other. The `pc` board looks up the same facts instead (@bare-pc).

== Interrupts and timers

`TimerInterrupt` extends `Platform` with what preemption needs: `init_timer(on_tick)`, `set_deadline`, `interrupts_on`, `interrupts_off`, and two provided methods, `restore_interrupts` and `without_interrupts`, the critical section that the heap and the page code use.

The contract that makes preemption possible is that a handler runs *on the interrupted stack, with every register saved*. It may then switch tasks and return much later. All three entries save the full state for that reason, including, on riscv64 and aarch64, all the floating-point registers, since only some are callee-saved.

=== riscv64

The timer belongs to M-mode, so S-mode asks OpenSBI for a deadline with the SBI TIME extension and takes a supervisor timer interrupt when it passes. One entry, `green_tea_trap_entry`, takes interrupts and faults. A fault may be a stack overflow, with `sp` in a guard page, where pushing a frame would fault forever, so the first instructions tell them apart using nothing but `sscratch` and `t0`:

#srcfile("green-tea-bare/src/platform/riscv64.rs", lines: ((242, 245), (250, 250), (264, 265), (279, 280),), notes: ((244, [`scause`'s top bit is set for an interrupt, so a non-negative value is a fault, handled at label 9 on a stack of its own.]), (250, [`TrapFrame` is 544 bytes: 32 integer registers, `sepc`, `sstatus`, `fcsr`, padding, 32 FP registers.]),))[```rust
csrw sscratch, t0
csrr t0, scause
bgez t0, 9f
csrr t0, sscratch
// …
addi sp, sp, -544
// …
mv a0, sp
call green_tea_trap         # may switch tasks, and return here much later
// …
addi sp, sp, 544
sret
```]

=== aarch64

Sixteen vectors of `0x80` bytes each save `x0` and `x1`, load their own index, and join a common path that pushes an 800-byte `Frame`, including all of `q0`–`q31`. Index 5 is an IRQ at the current level. The handler acknowledges the GIC and ends the interrupt *before* calling `on_tick`, since `on_tick` may switch tasks and the GIC sends nothing else of that priority until the end of interrupt:

#srcfile("green-tea-bare/src/platform/aarch64.rs", lines: ((332, 352),))[```rust
extern "C" fn green_tea_exception(kind: u64, frame: &Frame) {
    if kind == VECTOR_IRQ {
        let iar = unsafe { read_volatile(GICC_IAR as *const u32) };
        let id = iar & 0x3ff;
        if id == SPURIOUS {
            return;
        }
        // End the interrupt now, not after on_tick: on_tick may switch tasks and not
        // return for a long time, and until the end of interrupt the GIC sends no other
        // interrupt of the same priority. The timer's line stays high until the next
        // set_deadline, but interrupts stay masked until then.
        gic_write(GICC_EOIR, iar);
        if id == TIMER_IRQ {
            match ON_TICK.get() {
                Some(on_tick) => on_tick(),
                None => AarchVirt::set_deadline(u64::MAX),
            }
            return;
        }
        panic!("unexpected interrupt {id}");
    }
```]

The boot CPU installs the vectors in `init`, before anything else can fault:

#srcfile("green-tea-bare/src/platform/aarch64.rs", lines: ((89, 92),), notes: ((90, [`VBAR_EL1`: from here on a fault on this CPU reaches `green_tea_exception` and is reported.]),))[```rust
fn init() {
    unsafe { asm!("msr vbar_el1, {}", "isb", in(reg) &raw const green_tea_vectors) };
    Pl011(0x0900_0000).enable();
}
```]

A started CPU does the same in `green_tea_secondary`.

=== x86_64

`traps::init` builds a GDT with one TSS per CPU, an IDT with stubs for all 32 exceptions, and masks the legacy PIC after moving it to vectors `0x20`–`0x2f`. Exceptions run on IST1, a 16 KiB fault stack that each CPU has to itself, because an overflow faults with `rsp` in a guard page. The timer runs on the interrupted stack, through vector `0x30`, and the local APIC's spurious interrupt has an entry of its own at `0xff` that returns at once.

The local APIC is reached in one of two modes. In *xAPIC* mode its registers are memory at `0xfee0_0000`, or wherever `set_lapic_base` moved them. In *x2APIC* mode they are MSRs, and ids are 32 bits wide. `lapic_read`, `lapic_write`, `apic_id` and `send_ipi` hide the difference. `q35` stays in xAPIC mode, and `pc` uses x2APIC whenever the CPU has it.

The local APIC timer counts at a rate of its own, which `init_timer` measures against `Board::ticks` over 10 ms: the HPET on `q35`, the TSC on `pc`. It can't take back an interrupt it has raised, so a tick can be stale, or a little early from the conversion. The handler checks, and re-arms:

#srcfile("green-tea-bare/src/platform/x86_64/mod.rs", lines: ((665, 681),), notes: ((667, [Acknowledge first: `on_tick` may not come back for a long time.]), (673, [A stale or early tick: wait for the deadline that is armed now.]),))[```rust
extern "C" fn green_tea_timer() {
    // Acknowledge first: on_tick may switch tasks and not come back for a long time.
    lapic_write(LAPIC_EOI, 0);
    // The APIC can't take back an interrupt it has raised, so a tick can be stale: one
    // that fired while interrupts were off, before a later `set_deadline`. It can also be
    // a little early, from converting the deadline to APIC ticks. Either way, wait for the
    // deadline that's armed now.
    let deadline = DEADLINE.get();
    if Board::ticks() < deadline {
        Board::set_deadline(deadline);
        return;
    }
    DEADLINE.set(u64::MAX); // one tick per deadline
    if let Some(on_tick) = ON_TICK.get() {
        on_tick();
    }
}
```]

== Paging and guard pages

The map is the identity on every board: a page at an address gets the frame at that address. The drivers depend on it, because they hand heap addresses to devices that know nothing of page tables. What paging adds is *holes*.

riscv64 builds Sv39 tables in `paging::enable`: a 1 GiB leaf for the devices, a table of 2 MiB leaves for the gigabyte of RAM, and a table of 4 KiB leaves for the 2 MiB of stacks, in which each guard page is zero:

#srcfile("green-tea-bare/src/platform/riscv64.rs", lines: ((476, 487),), notes: ((476, [Entry 0 of the root maps the first GiB as device memory: readable and writable, not executable.]), (486, [Not valid: any access faults.]),))[```rust
(*root).0[0] = leaf(0, DEVICE); // the first GiB: UART, fw_cfg, CLINT, PLIC
(*root).0[RAM / GIB] = branch(ram);
for (i, entry) in (*ram).0.iter_mut().enumerate() {
    *entry = leaf(RAM + i * MIB2, MEMORY);
}
(*ram).0[(stacks - RAM) / MIB2] = branch(pool);
for (i, entry) in (*pool).0.iter_mut().enumerate() {
    *entry = leaf(stacks + i * stack::PAGE, MEMORY);
}
for guard in stack::guard_pages() {
    (*pool).0[(guard - stacks) / stack::PAGE] = 0; // not valid: any access faults
}
```]

`A` and `D` are set up front in every leaf, since QEMU otherwise faults the first access to have them set. x86_64 does the same in `enable_guard_pages`, replacing the 2 MiB entry of the stack pool in `_start`'s directories, and reloading `cr3`. Above the 4 GiB that `_start` maps, `paging::map_high` adds identity-mapped 2 MiB pages for a device that firmware put there, from eight spare tables; only `pc` calls it. aarch64 runs with the MMU off, so its guard pages are ordinary memory.

The pool is `stack::SLOTS` (102) slots of `SLOT` bytes (20 KiB): a `PAGE` of guard under a `STACK_SIZE` of 16 KiB. It is a `green-tea-pool` `FreeList` (@crate-pool), whose link goes in the first word *above* the guard, which a board with guard pages can't write. `Stack::take` panics when the pool is empty, `Stack::try_take` returns `None`, `top` and `guard` give the addresses, and dropping a `Stack` gives it back.

A fault in a guard page reaches `stack::overflowed`, which panics, or calls whatever `on_overflow` installed. `guard_slot` says which slot an address belongs to, and `guard_pages` lists them all. The `overflow` kernel recurses until it hits one:

#output(title: "cargo xtask kernel --kernel overflow --target riscv64")[
```
overflowing a 16 KiB task stack
PASS: overflowed after 26 frames, into the task's guard page at 0x80a05ff0
```
]

== Demand paging

On riscv64 and x86_64 the page tables start with all 1,024 pages of the heap missing. The first access faults, the handler maps the page, and the instruction runs again:

#srcfile("green-tea-bare/src/pages.rs", lines: ((118, 129),), notes: ((122, [`set_mapped` returns whether the page was mapped before.]), (124, [A fault on a page that is there is a real fault, and the handler panics.]),))[```rust
pub fn fault(addr: usize) -> bool {
    if !Board::DEMAND_PAGING || !heap_range().contains(&addr) {
        return false;
    }
    let was_mapped = Board::set_mapped(addr & !(PAGE - 1), true);
    if was_mapped {
        return false; // a fault on a page that is there: not for want of a mapping
    }
    FAULTS.fetch_add(1, Relaxed);
    count_mapped();
    true
}
```]

The heap can straddle three 2 MiB pages, so each board keeps three tables of 4 KiB entries, `HEAP_4K`, mapping what else shares those 2 MiB as before and leaving the heap's pages out:

#srcfile("green-tea-bare/src/platform/x86_64/mod.rs", lines: ((778, 792),), notes: ((786, [Only the tables the heap reaches. A heap on a 2 MiB boundary needs two, and the third entry, the stack pool's, stays as `enable_guard_pages` left it.]),))[```rust
pub fn unmap_heap() {
    let heap = pages::heap_range();
    let base = heap.start & !(MIB2 - 1);
    assert!(heap.end <= base + HEAP_TABLES * MIB2 && heap.end <= stack::region_start());
    HEAP_BASE.set(base);
    unsafe {
        // Only ever touched here, at boot: no other references exist.
        let (tables, pds) = (&raw mut HEAP_4K, &raw mut boot_pds);
        for (t, table) in (*tables).iter_mut().enumerate().take((heap.end - base).div_ceil(MIB2)) {
            for (i, entry) in table.0.iter_mut().enumerate() {
                let page = base + t * MIB2 + i * pages::PAGE;
                *entry = if heap.contains(&page) { 0 } else { page as u64 | PRESENT | WRITABLE };
            }
            (*pds)[base / MIB2 + t] = table as *mut Table as u64 | PRESENT | WRITABLE;
        }
```]

Unmapping needs a TLB flush on both boards. Mapping needs one on riscv64, whose TLB may hold an invalid entry, and not on x86_64, which never caches one.

`release` is the way back. It zeroes a page and unmaps it, so that a released page reads as zeros when next touched, as a page given back to Linux with `madvise(MADV_DONTNEED)` does. `trim` calls it on the pages of the heap's free blocks that hold none of the allocator's own words:

#srcfile("green-tea-bare/src/pages.rs", lines: ((223, 232),))[```rust
pub fn trim() -> usize {
    heap::with(|heap| {
        let mut released = 0;
        for range in idle_ranges(heap.blocks(), PAGE) {
            // SAFETY: the allocator reads nothing in a free block but those four words.
            released += unsafe { release(range) };
        }
        released
    })
}
```]

The rest of the module observes. `stats` returns a `Stats` of `reserved`, `mapped`, `peak`, `faults` and `released`; `is_mapped` and `mapped_in` ask about pages; `for_each_run` reports runs of mapped pages; `heap_range` is the heap's addresses. `walk` does what the CPU does on a TLB miss, in software, and returns one `Step` per level, each with an `index`, an `entry`, and a `PointsTo`: `Nothing`, `Table` or `Page`. The `pages` kernel uses it on three of the addresses that matter:

#output(title: "cargo xtask kernel --kernel pages --target riscv64")[
```
  0x80482000, the heap's first page
    entry   2  0x00000000200cfc01  the next table, at 0x8033f000
    entry   2  0x00000000200d0001  the next table, at 0x80340000
    entry 130  0x00000000201208cf  a page of 4 KiB, in the frame at 0x80482000
  0x80483000, the heap's second page
    entry   2  0x00000000200cfc01  the next table, at 0x8033f000
    entry   2  0x00000000200d0001  the next table, at 0x80340000
    entry 131  0x0000000000000000  not valid: an access faults
  0x80a00000, the guard page of the first task stack
    entry   2  0x00000000200cfc01  the next table, at 0x8033f000
    entry   5  0x00000000200d1001  the next table, at 0x80344000
    entry   0  0x0000000000000000  not valid: an access faults
```
]

And a megabyte's life, in pages:

#output(title: "cargo xtask kernel --kernel pages --target riscv64")[
```
                                         footprint  mapped  faults  released   the pages mapped
  after booting                                  0       2       2         0   0, 1023
  a megabyte allocated                         257       3       3         0   0, 256, 1023
  a byte written in each of 16 pages           257      18      18         0   0-15, 256, 1023
  every byte written                           257     258     258         0   0-256, 1023
  freed                                        257     258     258         0   0-256, 1023
  trimmed                                      257       2     258       256   0, 1023
```
]

Allocating a megabyte maps one page, 256, where its footer and the next block's header are. The heap's last page, 1023, is mapped from boot because the allocator's end marker is there.

== SMP startup <bare-smp>

A kernel begins on one CPU, CPU 0. `cpus::start_all` starts the others, up to `MAX` in all. `MAX` is 8 unless the build sets `GREEN_TEA_MAX_CPUS`, which `build.rs` accepts from 1 to 256. Each CPU the limit allows costs a 16 KiB fault stack in `.bss`:

#srcfile("green-tea-bare/src/cpus.rs", lines: ((59, 81),), notes: ((64, [Demand paging is for one CPU: plain counters, and a TLB flush that reaches one CPU. So the heap is mapped whole first, and `release` does nothing from then on.]), (68, [An error means no such CPU: the loop ends, and `count` is how many there are.]),))[```rust
pub fn start_all() -> usize {
    assert_eq!(Board::cpu(), 0, "only the boot CPU starts the others");
    if PARKED.load(Ordering::Acquire) {
        return count();
    }
    crate::pages::map_all(); // a page fault is something only the boot CPU can take
    for (cpu, online) in ONLINE.iter().enumerate().skip(count()) {
        let stack = Box::leak(vec![0u128; STACK / 16].into_boxed_slice());
        let top = stack.as_mut_ptr() as usize + STACK;
        if Board::start_cpu(cpu, top).is_err() {
            // SAFETY: the board started nothing, so the stack is still only ours.
            drop(unsafe { Box::from_raw(stack) });
            break;
        }
        let deadline = Board::ticks() + 1000 * Board::ticks_per_ms();
        while !online.load(Ordering::Acquire) {
            assert!(Board::ticks() < deadline, "CPU {cpu} was started and didn't arrive");
            spin_loop();
        }
        COUNT.store(cpu + 1, Ordering::Release);
    }
    count()
}
```]

Each board starts a CPU its own way:

- *riscv64*: the SBI HSM extension's `hart_start`. CPUs 1, 2, … are the harts other than the boot hart, in order of hart id. The new hart's number is written under the top of its stack, and `green_tea_secondary_entry` loads it into `tp`.
- *aarch64*: PSCI `CPU_ON` through `hvc`, which QEMU answers itself when there is no firmware. The CPU's number is the low byte of `MPIDR_EL1`.
- *x86_64*: messages from the local APIC, after copying a real-mode trampoline to `0x8000`. `q35` knows how many CPUs there are from fw_cfg, and a CPU's number is its APIC id:

#srcfile("green-tea-bare/src/platform/x86_64/q35.rs", lines: ((100, 109),), notes: ((102, [fw_cfg item 5: the number of CPUs QEMU was started with. INIT and STARTUP get no answer, so nothing else says whether the CPU exists.]), (107, [The destination is an APIC id, and on this board `cpu` is one.]),))[```rust
fn start_cpu(cpu: usize, stack_top: usize) -> Result<(), &'static str> {
    use green_tea_fw_cfg::FwCfgPort;
    IoPorts.select(0x05);
    let cpus = u16::from_le_bytes([IoPorts.read_u8(), IoPorts.read_u8()]) as usize;
    if cpu >= cpus {
        return Err("no such CPU");
    }
    super::wake_cpu(cpu as u32, stack_top);
    Ok(())
}
```]

`wake_cpu`, which both x86_64 boards share, sends INIT and then STARTUP twice, as Intel's startup sequence says:

#srcfile("green-tea-bare/src/platform/x86_64/mod.rs", lines: ((540, 547),), notes: ((544, [The vector is the page the CPU starts at.]), (547, [A CPU that took the first STARTUP ignores this one.]),))[```rust
lapic_write(LAPIC_SPURIOUS, APIC_ENABLE | SPURIOUS_VECTOR as u32);
send_ipi(apic_id, INIT);
crate::delay_ms(10);
// Twice, as Intel's startup sequence says: a CPU that took the first ignores the second.
let startup = STARTUP | (AP_TRAMPOLINE >> 12) as u32; // the page to start at
send_ipi(apic_id, startup);
crate::delay_ms(1);
send_ipi(apic_id, startup);
```]

The trampoline loads a GDT, enters protected mode and jumps to `ap_start32`, which repeats what `_start` did, with the page tables already built. `green_tea_secondary` then moves the CPU's local APIC to x2APIC mode if the boot CPU's is in it, and `traps::join` loads the boot CPU's GDT and IDT and this CPU's own TSS.

A started CPU then waits in `enter`, polling for work. `cpus::run` hands every CPU the same `fn(usize)`, itself included, and returns when all have finished. `together` holds them at a barrier so that none begins before all have arrived:

#srcfile("green-tea-bare/src/cpus.rs", lines: ((85, 106),))[```rust
pub fn run(f: fn(usize)) {
    assert_eq!(Board::cpu(), 0, "only the boot CPU hands out work");
    ARRIVED.store(0, Ordering::Release);
    for work in &WORK[1..count()] {
        work.store(f as usize, Ordering::Release);
    }
    together(f, 0);
    for work in &WORK[1..count()] {
        while work.load(Ordering::Acquire) != 0 {
            spin_loop();
        }
    }
}

/// Wait until every CPU is here, then run `f`.
fn together(f: fn(usize), cpu: usize) {
    ARRIVED.fetch_add(1, Ordering::AcqRel);
    while ARRIVED.load(Ordering::Acquire) < count() {
        spin_loop();
    }
    f(cpu);
}
```]

`count` is how many CPUs are running, and `park` stops the started ones for good. What a started CPU may use is short: the heap, the counter, atomics, and what its function brings. It runs with interrupts off and has no tasks. The scheduler, the console and every `Global` belong to the boot CPU.

`Global<T>` is what replaces `thread_local!`: a `Cell` in a static, with `get`, `set` and `replace`, and an `unsafe impl Sync` whose justification is that rule.

== The heap

The kernel's heap is a `Tlsf` (@crate-fit) over the 4 MiB of `.heap`, installed as the `#[global_allocator]`, so it is what `Box` and `Vec` allocate from. `NAME` is the string `"TLSF"`, for the tests to print. It is built on first use, out of line, because a `Tlsf` is 3 KiB of table and built inline it would sit in the frame of every allocation on a 16 KiB stack.

Every operation runs with interrupts off and holding a spin lock:

#srcfile("green-tea-bare/src/heap.rs", lines: ((124, 150),), notes: ((126, [The lock holds the owner's CPU number plus one, so that 0 can mean free.]), (130, [A use of the heap from inside itself is a panic, not a wait that never ends.]),))[```rust
fn locked<R>(f: impl FnOnce(&mut Tlsf, &mut Option<Recording>) -> R) -> R {
    Board::without_interrupts(|| {
        let me = Board::cpu() + 1;
        let mut waited = false;
        // Write our number if the lock holds 0, in one step no other CPU can come between.
        while let Err(owner) = OWNER.compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed) {
            assert!(owner != me, "the heap was used from inside heap::with");
            waited = true;
            spin_loop();
        }
        // We hold the lock, so nobody else is writing the count.
        TAKEN.store(TAKEN.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        if waited {
            WAITED.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: interrupts are off and the lock is ours, so this is the only reference.
        let slot = unsafe { &mut *HEAP.fit.get() };
        if slot.is_none() {
            start(slot);
        }
        let fit = slot.as_mut().expect("the heap was started above");
        // SAFETY: as for `fit`.
        let result = f(fit, unsafe { &mut *HEAP.recording.get() });
        OWNER.store(0, Ordering::Release);
        result
    })
}
```]

`with` runs a closure on the allocator, which must not allocate. `used` and `capacity` are its two commonest uses, and `lock_counts` returns how many times the lock was taken and how many of those waited for another CPU. `align_up` is a mask where `next_multiple_of` would be a division.

The heap can also record itself. `record` installs a buffer, every allocation and free is then written to it as a `green-tea-fit` trace `Event`, and `stop_recording` hands back a `Recording`: the `events`, their `len`, and how many were `dropped` when the buffer filled. `is_recording` says whether one is under way.

Under the heap, `kmem` holds the kernel's instance of `green-tea-kmem`'s page frames (@crate-kmem): `REGION` (16 MiB) of RAM from `__kernel_end` up, or `PAGES` frames, with a table in the first `TABLE_PAGES` that are never handed out. `region` gives the addresses, `with_frames` runs a closure on the allocator with interrupts off, and `KernelCache` is a slab `Cache` that can be a `static`, with `new`, `allocate`, `free`, `reap` and `with`. On `q35` and `pc`, where a write past RAM would go unnoticed, the first use checks the region's end against `Board::ram_end` and panics if it doesn't fit. On riscv64 and aarch64 an access past RAM faults, and the fault is reported.

`Arena` is a bump allocator over one block of the heap, and an `Allocator`, so `Vec::new_in(&arena)` works. An arena of no bytes takes no block. `reset` takes `&mut self`, so the borrow checker refuses to reset an arena while anything allocated from it is alive. `used`, `peak` and `capacity` report on it. Freeing does something only for the last block, and growth is in place only for the last block:

#srcfile("green-tea-bare/src/arena.rs", lines: ((93, 98), (102, 106),), notes: ((103, [`is_last`: this block ends where the next allocation would start.]),))[```rust
fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
    let addr = align_up(self.next.get(), layout.align()).ok_or(AllocError)?;
    self.bump_to(addr.checked_add(layout.size()).ok_or(AllocError)?)?;
    let ptr = NonNull::new(addr as *mut u8).ok_or(AllocError)?;
    Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
}
// …
unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
    if self.is_last(ptr, layout.size()) {
        self.next.set(ptr.as_ptr() as usize);
    }
}
```]

== The console

`Console` implements `fmt::Write` over `Board::putc`, turning each `\n` into `\r\n`, and the `print!` and `println!` macros write to it. Input is polled: a task waiting in `read_byte` sleeps a millisecond at a time, so that other tasks run:

#srcfile("green-tea-bare/src/console.rs", lines: ((16, 23),))[```rust
pub fn read_byte() -> u8 {
    loop {
        if let Some(byte) = Board::getc() {
            return byte;
        }
        task::sleep_ms(POLL_MS);
    }
}
```]

== The screen

`Screen` draws the demos on the board's `Framebuffer`: a status bar, and `COLUMNS` (5) columns, each with a bar that grows up to `BAR_MAX` pixels in its colour from `COLORS` on `BACKGROUND`. `new` clears it, `switch_to` moves a white outline to the task about to run and shows the switch count and task id, `idle` drops the outline, and `finish` turns the status bar green. The scheduler's hook owns the `Screen`. Workers draw through a `Painter`, a `Copy` that `painter` returns, with `bar` and `label`, which is safe because each worker draws only in its own column.

== Tasks and the scheduler

`task` is `green-tea-v2`'s task (@crate-v2), ported. A `Task` is an asymmetric coroutine: someone `resume`s it, it runs until it yields or finishes, and control returns to whoever resumed it. The switch is `green-tea-arch`'s `swap_context` (@crate-arch):

#srcfile("green-tea-bare/src/task.rs", lines: ((115, 121), (147, 151),), notes: ((118, [`CURRENT` is a `Global`, where v2 had a `thread_local!`.]),))[```rust
    pub fn resume(&mut self) {
        let inner = self.inner.as_ptr();
        assert!(!self.is_done(), "resumed a task that has already finished");
        let previous = CURRENT.replace(inner);
        unsafe { swap_context(&raw mut (*inner).caller, &raw const (*inner).context) };
        CURRENT.set(previous);
    }
// …
pub fn yield_now() {
    let inner = CURRENT.get();
    assert!(!inner.is_null(), "yield_now called outside a task");
    unsafe { swap_context(&raw mut (*inner).context, &raw const (*inner).caller) };
}
```]

A task starts in `task_entry`, on its own stack, runs its closure, marks itself done, and switches back for the last time:

#srcfile("green-tea-bare/src/task.rs", lines: ((182, 192),))[```rust
extern "C" fn task_entry(arg: *mut ()) -> ! {
    let inner: *mut Inner = arg.cast();
    unsafe {
        if let Some(func) = (*inner).func.take() {
            func(); // a panic here goes to the panic handler, which halts
        }
        (*inner).done = true;
        swap_context(&raw mut (*inner).context, &raw const (*inner).caller);
    }
    unreachable!("a finished task was resumed")
}
```]

A task's record comes from a slab cache, `TASKS`, and a constructed record *keeps its stack*. So a task that is created takes a stack from the pool only when the cache has to make a new slab, and a thousand tasks run one after another use one stack. `reap` destroys the records no task is using, which returns their stacks and pages, and `cache` looks at the cache. `id`, `is_done` and `take_wake_at` read a task's state; `in_task` says whether a task is running; `current_guard` is its guard page. `sleep_until` sets a wake time and yields, and `sleep_ms` is the same with a relative time.

`runtime` is the round-robin scheduler:

#srcfile("green-tea-bare/src/runtime.rs", lines: ((48, 69),), notes: ((52, [A separate statement, so the queue's borrow ends before the task runs: the task may call `spawn`.]), (57, [Everything is asleep: the scheduler spins until the first deadline.]), (60, [The hook sees each `Switch`: `To(id)`, or `Idle`. The demos draw here, and `green-tea-preempt` arms its time slice.]),))[```rust
loop {
    runtime.wake(Board::ticks());
    // A separate statement, so the queue borrow ends before the task runs: the task
    // may call `spawn`, which borrows the queue again.
    let next = runtime.queue.borrow_mut().pop_front();
    let Some(mut task) = next else {
        let first_due = runtime.sleeping.borrow().iter().map(|&(due, _)| due).min();
        let Some(due) = first_due else { break }; // nothing runnable or asleep: done
        on_switch(Switch::Idle);
        while Board::ticks() < due {}
        continue;
    };
    on_switch(Switch::To(task.id()));
    task.resume();
    if task.is_done() {
        continue;
    }
    match task.take_wake_at() {
        Some(due) => runtime.sleeping.borrow_mut().push((due, task)),
        None => runtime.queue.borrow_mut().push_back(task),
    }
}
```]

`run` takes the first task and a hook, and returns when no task is left. `spawn` adds to the back of the queue, from `run`'s `main` or any task. A sleeping task is off the queue, so it costs no time slices.

== The `pc` board <bare-pc>

`Pc`, in #repo("green-tea-bare/src/platform/x86_64/pc.rs"), is the board for a physical x86_64 PC, chosen at build time by the `board-pc` feature, which `cargo xtask kernel --board pc` turns on. Without the feature the x86_64 build is `Q35`. The two share `_start`, the traps, the local APIC and the paging in `mod.rs`. What `Q35` knows by address, `Pc` looks up:

#dtable(columns: (auto, 1fr, 1fr), ("", "q35", "pc"),
  [Loaded by], [QEMU, through PVH], [GRUB from UEFI, through Multiboot2],
  [Load address], [`0x100000`], [`0x2000000` (32 MiB)],
  [RAM], [fw_cfg item 3], [the Multiboot2 memory map],
  [Screen], [`ramfb`], [GRUB's framebuffer, any size],
  [`ticks`], [HPET], [TSC, measured at boot],
  [CPUs], [fw_cfg item 5], [ACPI's MADT],
  [CPU number], [APIC id], [position in the MADT's list],
  [Local APIC], [xAPIC], [x2APIC if the CPU has it],
  [Console], [COM1], [the screen, and COM1 if it answers],
  [Keyboard], [the serial port], [the serial port, USB and PS/2],
  [Disk], [virtio-blk], [NVMe],
  [Test to run], [fw_cfg file], [`test=` on GRUB's command line],
)

=== Booting

GRUB looks for a Multiboot2 header in the first 32 KiB of the file. The linker script puts the section `.multiboot2` first:

#srcfile("green-tea-bare/src/platform/x86_64/pc.rs", lines: ((42, 53),), notes: ((47, [Tag 5, the framebuffer, with the flag that makes it optional.]), (49, [Any width and height, 32 bits per pixel.]),))[```rust
mb2_start:
    .long 0xe85250d6
    .long 0
    .long mb2_end - mb2_start
    .long 0x100000000 - (0xe85250d6 + (mb2_end - mb2_start))
    .short 5, 1
    .long 20
    .long 0, 0, 32
    .balign 8
    .short 0, 0
    .long 8
mb2_end:
```]

GRUB enters `_start` in 32-bit protected mode with paging off, as PVH does, with a magic number in `eax` and the address of its boot information in `ebx`. The kernel is linked at 32 MiB because UEFI firmware keeps data of its own a few MiB up, where a kernel at 1 MiB would overlap it. From the memory map, `init` keeps the end of the free RAM the kernel is in, for the check in `kmem`, and whether the page at `0x8000` is free, since `start_cpu` refuses to start a CPU if it isn't. Then it turns to ACPI:

#srcfile("green-tea-bare/src/platform/x86_64/pc.rs", lines: ((325, 332), (340, 342),), notes: ((326, [Before the first `delay_ms`.]), (329, [Uncached, in the page tables.]), (331, [Before `init_cpus`, which reads this CPU's APIC id.]),))[```rust
let acpi = info.rsdp().and_then(|rsdp| Acpi::new(Identity4G, rsdp));
TSC_PER_MS.store(measure_tsc(acpi.as_ref()), Ordering::Relaxed);
if let Some(address) = acpi.as_ref().and_then(|a| a.lapic_address()) {
    set_lapic_base(address);
    paging::map_device(address as usize, 0x1000).unwrap_or_else(|e| panic!("local APIC: {e}"));
}
enable_x2apic_if_available();
init_cpus(acpi.as_ref());
// …
paging::enable_guard_pages();
paging::unmap_heap();
init_usb(acpi.as_ref());
```]

Unlike QEMU's, a physical machine's RAM doesn't start out zero, so `init` also clears the heap and the stack pool, which `_start` leaves alone.

=== The screen and the console

`init_screen` takes the framebuffer GRUB set up, if it is 32-bit XRGB, maps it with `map_high` when it is above 4 GiB, and clears what the firmware and GRUB left on it. The demos draw in the top-left 640×480. A text console, the `Text` of `green-tea-framebuffer` (@crate-framebuffer), takes the rest: beside the demos on a screen at least 1,024 pixels wide, below them on one at least 600 high, and over them on anything smaller. `putc` writes every byte to that console, and to COM1 if there is one. `init_serial` decides that by writing two values to the UART's scratch register and reading them back, since a laptop has no UART and its ports read `0xff`. A flag guards the console: a CPU or interrupt that finds it taken leaves its character off the screen, so that a `panic!` can't deadlock on it.

A screen is optional. With no framebuffer, or one that isn't XRGB, the kernels draw into 640×480 of plain memory and report through the serial port:

#srcfile("green-tea-bare/src/platform/x86_64/pc.rs", lines: ((413, 424),), notes: ((418, [A static, so that every kernel has somewhere to draw.]),))[```rust
fn display() -> Result<Framebuffer, &'static str> {
    // SAFETY: only `init` writes it, before this can be called.
    let Some(fb) = (unsafe { (&raw const SCREEN).read() }) else {
        // A machine can run without a screen, so the demos draw into plain memory.
        crate::println!("framebuffer: none usable, drawing to memory");
        let pixels = (&raw mut OFFSCREEN).cast::<u32>();
        // SAFETY: a static of this shape, and every call shares the one set of pixels.
        return Ok(unsafe { Framebuffer::new(pixels, OFFSCREEN_W, OFFSCREEN_H, OFFSCREEN_W) });
    };
    crate::println!("framebuffer: {}x{} XRGB8888", fb.width(), fb.height());
    Ok(fb)
}
```]

=== The clock

The TSC is the counter `ticks` reads, and nothing says how fast it runs, so `measure_tsc` times it for 10 ms against a clock whose rate ACPI (@crate-acpi) gives. That is the HPET if the firmware lists one below 4 GiB, and otherwise the ACPI power management timer, at 3,579,545 Hz, of which only 24 bits are sure to exist:

#srcfile("green-tea-bare/src/platform/x86_64/pc.rs", lines: ((161, 169),))[```rust
} else if let Some(port) = acpi.pm_timer_port() {
    const MASK: u32 = 0x00ff_ffff; // 24 bits is all a timer is sure to have
    let wait = (PM_TIMER_HZ * CALIBRATE_MS / 1000) as u32;
    // SAFETY: ACPI's port for the timer; reading it has no effect.
    unsafe {
        let (from, start) = (inl(port) & MASK, rdtsc());
        while (inl(port) & MASK).wrapping_sub(from) & MASK < wait {}
        (start, rdtsc())
    }
```]

=== The CPUs

The MADT lists the local APICs, and on a physical machine their ids can have gaps. So a CPU's number is its position in a list, with the boot CPU first:

#srcfile("green-tea-bare/src/platform/x86_64/pc.rs", lines: ((244, 254),), notes: ((249, [At most `MAX` ids, in the MADT's order, leaving out the boot CPU's, which is already first.]),))[```rust
fn init_cpus(acpi: Option<&Acpi<Identity4G>>) {
    let boot = apic_id();
    APIC_IDS[0].store(boot, Ordering::Relaxed);
    let Some(acpi) = acpi else { return };
    let mut ids = [0; MAX];
    let count = acpi.apic_ids(boot, &mut ids).max(1);
    for (slot, id) in APIC_IDS.iter().zip(&ids[..count]) {
        slot.store(*id, Ordering::Relaxed);
    }
    CPU_COUNT.store(count, Ordering::Release);
}
```]

`cpu` searches that list for this CPU's APIC id, and `start_cpu` passes `wake_cpu` the id at position `cpu`. The list is written once, before any other CPU starts. `enable_x2apic_if_available` moves the boot CPU to x2APIC whenever CPUID reports it, because firmware may already have turned it on and it can't be turned off without resetting the APIC's state. Ids above 255 need that mode.

=== Device memory

`_start` maps everything write-back. On a PC the firmware's MTRRs normally make device registers uncached, and we don't rely on them: `map_device` sets `PCD` and `PWT` on the 2 MiB pages a device is in. With the PAT at its reset value, that combination selects uncacheable. The board calls it for the HPET, the local APIC, the ECAM window and each controller's BAR. A page that holds any part of the kernel is refused:

#srcfile("green-tea-bare/src/platform/x86_64/mod.rs", lines: ((840, 866),), size: 6.4pt, notes: ((841, [Bits 3 and 4 of a page entry.]), (847, [A device above 4 GiB gets new entries, with the same bits.]), (858, [A heap or stack page has already been split into a table, and it counts as the kernel too.]),))[```rust
pub fn map_device(addr: usize, len: usize) -> Result<(), &'static str> {
    const UNCACHED: u64 = 1 << 3 | 1 << 4;
    const PAGE_SIZE: u64 = 1 << 7;
    unsafe extern "C" {
        fn _start();
        static __kernel_end: u8;
    }
    map_above_4g(addr, len, UNCACHED)?;
    if addr >= 1 << 32 {
        return Ok(());
    }
    let kernel = (_start as *const () as usize & !(MIB2 - 1))..(&raw const __kernel_end as usize);
    let (first, last) = (addr & !(MIB2 - 1), (addr + len - 1).min((1 << 32) - 1) & !(MIB2 - 1));
    // SAFETY: at boot, on the boot CPU; each index is within the 2048 entries.
    unsafe {
        let pds = (&raw mut boot_pds).cast::<u64>();
        for page in (first..=last).step_by(MIB2) {
            let entry = pds.add(page / MIB2);
            if kernel.start < page + MIB2 && page < kernel.end || entry.read() & PAGE_SIZE == 0 {
                return Err("a device shares a 2 MiB page with the kernel");
            }
            entry.write(entry.read() | UNCACHED);
        }
        asm!("mov {0}, cr3", "mov cr3, {0}", out(reg) _, options(nostack));
    }
    Ok(())
}
```]

In QEMU's page tables, after a boot, the local APIC's page reads `--P-ACT-W`, where `C` and `T` are those two bits.

=== The keyboards

`getc` polls three sources in order: COM1 if it answered, then a USB keyboard, then a PS/2 one. `init_usb` finds the xHCI controllers through ACPI's MCFG table and keeps the first with a boot keyboard on one of its own ports (@crate-xhci). PS/2 is `green-tea-i8042` (@crate-i8042), which is what most laptops' built-in keyboards are. Neither takes an interrupt.

=== The disk

`Pc::find_nvme` walks the ECAM window that `init_usb` kept for the first function of class `0x010802`, maps its BAR with `map_device`, enables it, and returns a started `Nvme` (@crate-nvme). It can be called once, and only while one CPU is running, since it changes the page tables. Only the `disk` kernel calls it.

=== A run

`cargo xtask kernel --board pc` builds GRUB and the kernel into a directory laid out as a UEFI system partition, and boots it in QEMU with OVMF firmware, a `qemu-xhci` controller and a USB keyboard:

#output(title: "cargo xtask kernel --board pc --kernel coop --headless")[
```
BdsDxe: starting Boot0001 "UEFI Misc Device" from PciRoot(0x0)/Pci(0x3,0x0)
usb: keyboard on port 5
green-tea-coop on x86_64 (pc)
framebuffer: 1280x800 XRGB8888
running 5 workers; expecting 57 switches
all tasks finished after 57 switches (expected 57)
PASS: 57 switches, as expected
```
]

== Limits <bare-limits>

- *The `pc` board has run only in QEMU with UEFI firmware and under KVM, never on physical hardware.* The run above is QEMU with OVMF, emulated, as `xtask` starts it. The conditions a physical machine would bring were forced in QEMU: no HPET, xAPIC and x2APIC, sparse APIC ids, 12 CPUs with a limit of 16, memory mapped above 4 GiB, and PS/2 and USB keyboards at full and high speed. A local APIC at a non-default address is covered only by a host test of its parsing.
- *Secure Boot must be off*, since the GRUB that `xtask` builds isn't signed.
- *USB is a keyboard plugged straight into a controller's port before boot*: no hubs, no hot-plugging, and no USB storage. The disk the board drives is NVMe, and it has run only against QEMU's `nvme` device.
- `pc` assumes the TSC runs at a constant rate and agrees between CPUs. It doesn't check CPUID for an invariant TSC.
- The framebuffer stays write-back, and `map_device` works in 2 MiB pages, so anything else in a device's page becomes uncached with it.
- *`q35` and the two `virt` boards assume their QEMU machine.* Addresses are constants, and `q35` needs fw_cfg for the CPU count and the display. It uses the CPU number as an APIC id (`q35.rs:107`), and aarch64 takes it from `MPIDR_EL1`'s low byte (`aarch64.rs:125`), which holds on QEMU, whose ids run 0, 1, 2, ….
- *A regression in the guard pages wouldn't show in CI.* `unmap_heap` on riscv64 and x86_64 must not touch the stack pool's 2 MiB page when the heap starts on a 2 MiB boundary. Only a build with the heap on that boundary exercises this, and the builds CI runs don't have one.
- The scheduler's idle wait is a busy loop (`runtime.rs:57`), and the serial port is polled.

Four boards, one trait, and the parts of a kernel. @crate-coop puts them to work.
