//! The kernel's memory map, as linker.ld lays it out: where each section starts and ends,
//! read from the symbols the linker script defines around it.
//!
//! A linker symbol has an address and no contents. `static __heap_start: u8` declares a
//! byte that nothing may read; only `&raw const __heap_start`, its address, means anything.
//!
//! Two of these symbols can have the same address: `__bss_end` and `__stack_bottom`, or
//! `__stacks_end` and `__kernel_end`. Rust and LLVM assume two different statics never do,
//! so an optimized build folds `stacks_end == kernel_end` to false without looking. Every
//! address therefore goes through [`address`], which hides where it came from.

use core::hint::black_box;
use core::ops::Range;

unsafe extern "C" {
    static __kernel_start: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
    static __bss_start: u8;
    static __bss_end: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
    static __heap_start: u8;
    static __heap_end: u8;
    static __stacks_start: u8;
    static __stacks_end: u8;
    static __kernel_end: u8;
}

/// The alignment of an instruction: what `.text` must start on.
#[cfg(target_arch = "riscv64")]
const TEXT_ALIGN: usize = 2; // with the C extension; 4 without
#[cfg(target_arch = "aarch64")]
const TEXT_ALIGN: usize = 4;
#[cfg(target_arch = "x86_64")]
const TEXT_ALIGN: usize = 1;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// One output section of linker.ld.
#[derive(Clone, Copy)]
pub struct Section {
    pub name: &'static str,
    pub start: usize,
    pub end: usize,
    /// The alignment linker.ld asks for (for `.text`, the least the CPU needs).
    pub align: usize,
    /// The size linker.ld reserves, for the sections it sizes itself.
    pub reserved: Option<usize>,
}

impl Section {
    pub fn size(&self) -> usize {
        self.end - self.start
    }

    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    pub fn contains(&self, addr: usize) -> bool {
        self.range().contains(&addr)
    }
}

/// A linker symbol's address, as a number the optimizer knows nothing about.
fn address(symbol: *const u8) -> usize {
    black_box(symbol as usize)
}

/// From the load address to the end of the last section.
pub fn kernel() -> Range<usize> {
    address(&raw const __kernel_start)..address(&raw const __kernel_end)
}

/// Every section, lowest first. The alignments and reserved sizes repeat linker.ld's, so
/// that a kernel can check the one against the other.
pub fn sections() -> [Section; 7] {
    macro_rules! section {
        ($name:literal, $start:ident, $end:ident, $align:expr, $reserved:expr) => {
            Section {
                name: $name,
                start: address(&raw const $start),
                end: address(&raw const $end),
                align: $align,
                reserved: $reserved,
            }
        };
    }
    [
        section!(".text", __text_start, __text_end, TEXT_ALIGN, None),
        section!(".rodata", __rodata_start, __rodata_end, 16, None),
        section!(".data", __data_start, __data_end, 16, None),
        section!(".bss", __bss_start, __bss_end, 16, None),
        section!(".stack", __stack_bottom, __stack_top, 4096, Some(64 * KIB)),
        section!(".heap", __heap_start, __heap_end, 4096, Some(4 * MIB)),
        section!(".stacks", __stacks_start, __stacks_end, 2 * MIB, Some(2 * MIB)),
    ]
}

/// The section `addr` is in.
pub fn section_of(addr: usize) -> Option<Section> {
    sections().into_iter().find(|s| s.contains(addr))
}
