//! QEMU `q35`, x86_64. QEMU boots the ELF through PVH: it finds the entry point in a Xen
//! ELF note and jumps there in 32-bit protected mode, paging off. `_start` then does what
//! a bootloader would: identity-map the first 4 GiB with 2 MiB pages, turn on long mode,
//! and far-jump into 64-bit code.
//!
//! The x86_64-unknown-none target is soft-float (no SSE), so nothing needs enabling for
//! floating point, and swap_context has no float registers to save anyway.

use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};

pub const NAME: &str = "x86_64";

global_asm!(
    r#"
    # PVH: "XEN_ELFNOTE_PHYS32_ENTRY" (type 18) holds the 32-bit entry point.
    .section .note.Xen, "a", @note
    .balign 4
    .long 4
    .long 4
    .long 18
    .asciz "Xen"
    .balign 4
    .long _start
    .balign 4

    .section .text.entry, "ax"
    .code32
    .globl _start
_start:
    cli
    cld

    # Zero .bss (the framebuffer, the statics and the page tables below).
    mov $__bss_start, %edi
    mov $__bss_end, %ecx
    sub %edi, %ecx
    shr $2, %ecx
    xor %eax, %eax
    rep stosl

    # PML4[0] -> PDPT; PDPT[0..4] -> four page directories; each directory entry maps a
    # 2 MiB page (0x83 = present | writable | page size). 2048 of them cover 4 GiB: RAM,
    # and the HPET at 0xfed00000.
    mov $boot_pdpt, %eax
    or $0x3, %eax
    mov %eax, boot_pml4
    mov $boot_pds, %eax
    or $0x3, %eax
    xor %ecx, %ecx
1:  mov %eax, boot_pdpt(,%ecx,8)
    add $0x1000, %eax
    inc %ecx
    cmp $4, %ecx
    jne 1b
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
3:  hlt
    jmp 3b

    .section .rodata.boot_gdt, "a"
    .balign 8
boot_gdt:
    .quad 0
    .quad 0x00af9a000000ffff    # 0x08: 64-bit code
    .quad 0x00cf92000000ffff    # 0x10: data
boot_gdt_end:
boot_gdt_ptr:
    .word boot_gdt_end - boot_gdt - 1
    .long boot_gdt

    .section .bss.boot_page_tables, "aw", @nobits
    .balign 4096
boot_pml4: .skip 4096
boot_pdpt: .skip 4096
boot_pds:  .skip 4 * 4096
"#,
    options(att_syntax)
);

unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nostack)) };
}

unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nostack)) };
    value
}

unsafe fn outw(port: u16, value: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nostack)) };
}

unsafe fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nostack)) };
}

const HPET: usize = 0xfed0_0000;
const HPET_CAPABILITIES: *const u64 = HPET as *const u64;
const HPET_CONFIG: *mut u64 = (HPET + 0x10) as *mut u64;
const HPET_COUNTER: *const u64 = (HPET + 0xf0) as *const u64;

/// Start the HPET's main counter, which `ticks` reads.
pub fn init() {
    unsafe { write_volatile(HPET_CONFIG, read_volatile(HPET_CONFIG) | 1) };
}

/// COM1, a 16550 on I/O ports: wait for room, write the byte.
pub fn putc(byte: u8) {
    const COM1: u16 = 0x3f8;
    const LSR_THR_EMPTY: u8 = 1 << 5;
    unsafe {
        while inb(COM1 + 5) & LSR_THR_EMPTY == 0 {}
        outb(COM1, byte);
    }
}

/// fw_cfg on x86 lives on I/O ports rather than memory-mapped registers.
pub struct FwCfg;
pub const FW_CFG: FwCfg = FwCfg;

impl FwCfg {
    pub fn select(&self, key: u16) {
        unsafe { outw(0x510, key) }; // the port selector is little-endian, unlike MMIO
    }

    pub fn read_u8(&self) -> u8 {
        unsafe { inb(0x511) }
    }

    /// The DMA address register is big-endian: high half at 0x514, then low half at 0x518,
    /// which starts the transfer.
    pub fn start_dma(&self, address: u64) {
        unsafe {
            outl(0x514, ((address >> 32) as u32).to_be());
            outl(0x518, (address as u32).to_be());
        }
    }
}

/// The HPET's main counter.
pub fn ticks() -> u64 {
    unsafe { read_volatile(HPET_COUNTER) }
}

pub fn ticks_per_ms() -> u64 {
    let period_fs = unsafe { read_volatile(HPET_CAPABILITIES) } >> 32; // femtoseconds per tick
    1_000_000_000_000 / period_fs
}

pub fn halt() -> ! {
    loop {
        unsafe { asm!("hlt") };
    }
}
