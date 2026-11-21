//! What green-tea's drivers share: [`dma_fence`], and on x86_64 the [`port`] I/O
//! instructions.
//!
//! The drivers assume what green-tea-bare provides: memory mapped one to one, so the
//! address of a buffer is also the physical address a device needs for DMA. They take no
//! locks: where a kernel starts several CPUs, keeping them from using one device at once
//! is the caller's job.

#![no_std]

#[cfg(target_arch = "x86_64")]
pub mod port;

/// Order this CPU's memory accesses around a device's, both ways: what we wrote to memory
/// is visible to a DMA-ing device before a later register write tells it to look, and
/// what it wrote is visible to us after we've seen (by a register or a ring index) that
/// it's done.
///
/// An ordinary `fence` isn't enough on every architecture. RISC-V orders device I/O and
/// memory separately (`fence rw, rw` says nothing about I/O); aarch64 needs a `dsb` for
/// its effect to reach the rest of the system. On x86_64, `mfence` orders everything.
/// All three also stop the compiler moving memory accesses across them.
#[inline]
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
