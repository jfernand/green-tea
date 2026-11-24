#import "../lib.typ": *

= green-tea-io <crate-io>

`green-tea-io` holds the two primitives the drivers share: `dma_fence`, which orders a CPU's memory accesses against a device's, and on `x86_64` the `port` module, the instructions that reach the I/O port space. It is not a driver and knows no device.

#spec(
  ("Source", [#repo("drivers/io/src/"), 2 files, 76 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-ns16550` (on `x86_64` only), `green-tea-fw-cfg`, `green-tea-virtio`, `green-tea-virtio-blk`, `green-tea-bare`]),
  ("Tests", [no host tests (`test = false`); exercised by every kernel, through the console on `x86_64` and fw_cfg on QEMU's boards]),
)

#dtable(columns: (auto, auto, 1fr), ("Item", "Where", "What it does"),
  [`dma_fence()`], [all targets], [a full barrier between CPU memory accesses and device accesses],
  [`port::outb`, `outw`, `outl`], [`x86_64`], [write 8, 16 or 32 bits to an I/O port],
  [`port::inb`, `inl`], [`x86_64`], [read 8 or 32 bits from an I/O port],
)

== `dma_fence`

A device that does DMA reads and writes guest memory on its own, so a driver has two orderings to guarantee: what it wrote is in memory before a register write tells the device to look, and what the device wrote is visible once a register or ring index says it's done. Each architecture needs a different instruction:

#srcfile("drivers/io/src/lib.rs", lines: ((24, 37),), notes: ((27, [`fence rw, rw` would not do: RISC-V orders device I/O and memory separately, so the fence names both, `i` and `o` included.]), (31, [`dsb` rather than `dmb`: the access has to complete in the rest of the system, not only be ordered among this CPU's accesses.]),))[```rust
pub fn dma_fence() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack, preserves_flags))
    };
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags))
    };
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mfence", options(nostack, preserves_flags))
    };
}
```]

None of the three `asm!` blocks declares `nomem`, so the compiler has to assume each one reads and writes memory, and can't move a load or store across it. The fence is a compiler barrier as well as a CPU one.

== The `port` module

`x86_64` has a second, 64 KiB address space next to memory, reached only with `in` and `out`. The serial port and fw_cfg live there on `q35`, and the ACPI power management timer on the `pc` board. The five functions are one instruction each.


All five are `unsafe`, because a port write has whatever effect the device gives it and a read can have one too: reading the 16550's data register pops its FIFO. The module is compiled only for `x86_64`, so code that names it has to be behind the same `cfg`.

== Exercised by

`dma_fence` runs in `green-tea-fw-cfg`'s DMA write, which every kernel on QEMU's boards makes at boot, and around each virtqueue operation in the `disk` kernel. On `x86_64`, every byte on the console goes through `inb` and `outb`, the fw_cfg selector through `outw`, and the DMA address through `outl`. `inl` has one caller, the `pc` board, which reads the power management timer with it on a machine without an HPET (@crate-acpi records that boot). This `disk` run on `x86_64` used the other four port functions and the fence:

#output(title: "cargo xtask kernel --kernel disk --target x86_64 --headless")[
```
green-tea-coop on x86_64
ramfb: 640x480 XRGB8888
virtio: block device (id 2)
PASS: read 64 sectors over virtio-PCI, and wrote one back
```
]

== Limits

- There is no `inw`: no driver needs it.
- `dma_fence` is the strongest barrier on each architecture, and the only one offered. A driver that needed only one direction pays for both.
- No driver takes a lock. Where a kernel starts several CPUs, keeping them from using one device at once is the caller's job, as the crate's documentation says.

One fence and five instructions. @crate-ns16550 is the first driver built on them.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: f9af6e74f80b
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
