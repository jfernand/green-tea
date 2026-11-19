//! QEMU `q35`, x86_64. QEMU boots the ELF through PVH: it finds the entry point in a Xen
//! ELF note and jumps there in 32-bit protected mode, paging off. `_start` then does what
//! a bootloader would: identity-map the first 4 GiB with 2 MiB pages, turn on long mode,
//! and far-jump into 64-bit code.
//!
//! The x86_64-unknown-none target is soft-float (no SSE), so nothing needs enabling for
//! floating point, and neither swap_context nor the interrupt entry has float registers to
//! save.
//!
//! Interrupts: `init` loads an IDT (and the GDT and TSS it needs), and masks the legacy
//! PIC. The timer interrupt comes from the local APIC's timer, in one-shot mode; it counts
//! at its own rate, which `init_timer` measures against the HPET that `ticks` reads.
//!
//! Guard pages: `init` also maps the task stack pool with 4 KiB pages, leaving out the
//! guard pages. Exceptions run on a stack of their own (IST1 in the TSS), since a stack
//! overflow faults with the task's stack pointer in a guard page.

use super::{Platform, TimerInterrupt};
use crate::Global;
use crate::stack;
use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};
use green_tea_fw_cfg::IoPorts;
use green_tea_io::port::outb;
use green_tea_ns16550::Ns16550Port;
use green_tea_pci::Ecam;
use green_tea_virtio::{self as virtio, AnyTransport};

pub struct Q35;

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
    .globl boot_pds             # paging::enable_guard_pages edits one entry
boot_pml4: .skip 4096
boot_pdpt: .skip 4096
boot_pds:  .skip 4 * 4096
"#,
    options(att_syntax)
);

const HPET: usize = 0xfed0_0000;
const HPET_CAPABILITIES: *const u64 = HPET as *const u64;
const HPET_CONFIG: *mut u64 = (HPET + 0x10) as *mut u64;
const HPET_COUNTER: *const u64 = (HPET + 0xf0) as *const u64;

impl Platform for Q35 {
    const NAME: &'static str = "x86_64";
    const GUARD_PAGES: bool = true;

    /// Start the HPET's main counter, which `ticks` reads. Catch exceptions (so a fault
    /// panics with a report instead of triple-faulting into a reset), and leave out the
    /// guard pages.
    fn init() {
        unsafe { write_volatile(HPET_CONFIG, read_volatile(HPET_CONFIG) | 1) };
        traps::init();
        paging::enable_guard_pages();
    }

    /// COM1, a 16550 on I/O ports.
    fn putc(byte: u8) {
        Ns16550Port(0x3f8).putc(byte);
    }

    fn getc() -> Option<u8> {
        Ns16550Port(0x3f8).getc()
    }

    /// The HPET's main counter.
    fn ticks() -> u64 {
        unsafe { read_volatile(HPET_COUNTER) }
    }

    fn ticks_per_ms() -> u64 {
        let period_fs = unsafe { read_volatile(HPET_CAPABILITIES) } >> 32; // femtoseconds per tick
        1_000_000_000_000 / period_fs
    }

    fn halt() -> ! {
        loop {
            unsafe { asm!("hlt") };
        }
    }

    type FwCfg = IoPorts;
    const FW_CFG: IoPorts = IoPorts;

    /// virtio devices are PCI functions here. SeaBIOS put ECAM at 0xb000_0000 (inside
    /// the 4 GiB that `_start` maps) and gave their BARs addresses below 4 GiB.
    fn for_each_virtio(mut f: impl FnMut(virtio::Device)) -> Result<(), &'static str> {
        let ecam = Ecam(0xb000_0000);
        ecam.for_each_function(|function| {
            if let Some((id, transport)) = virtio::pci::probe(ecam, function) {
                f(virtio::Device { id, transport: AnyTransport::Pci(transport) });
            }
        });
        Ok(())
    }
}

// ---- Traps ----------------------------------------------------------------------------

/// The local APIC's timer interrupt. 0x20..0x30 is where `init` moves the (masked) PIC.
const TIMER_VECTOR: usize = 0x30;
/// What the local APIC raises when an interrupt it was about to deliver goes away.
const SPURIOUS_VECTOR: usize = 0xff;
const RFLAGS_IF: u64 = 1 << 9; // interrupts on
const PAGE_FAULT: u64 = 14;

/// What an exception stub leaves on the fault stack: the vector and error code (0 for
/// exceptions without one), then what the CPU pushed.
#[repr(C)]
struct ExceptionFrame {
    vector: u64,
    error: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

global_asm!(
    r#"
    .section .text, "ax"

    # Exceptions 0-31: push the vector (after a dummy error code, for those without one),
    # so that one handler can tell them apart. They run on IST1, the fault stack.
    .irp n, 0,1,2,3,4,5,6,7,9,15,16,18,19,20,22,23,24,25,26,27,28,31
green_tea_exception_\n:
    push $0
    push $\n
    jmp green_tea_exception_common
    .endr
    .irp n, 8,10,11,12,13,14,17,21,29,30
green_tea_exception_\n:
    push $\n
    jmp green_tea_exception_common
    .endr

green_tea_exception_common:
    mov %rsp, %rdi              # the ExceptionFrame
    and $-16, %rsp              # the SysV ABI's alignment, for the call
    call green_tea_fault        # doesn't return
    ud2

    # The timer interrupt, on the interrupted task's stack. In 64-bit mode the CPU aligns
    # rsp to 16 before pushing its 5-word frame; 15 more words keep it aligned for the
    # call. Every register is saved, not just the caller-saved ones that an ordinary call
    # would clobber: the handler may switch tasks. (No red zone to skip: the target
    # disables it, so code may be interrupted with nothing live below rsp.)
    .globl green_tea_timer_entry
green_tea_timer_entry:
    .irp r, rax,rbx,rcx,rdx,rsi,rdi,rbp,r8,r9,r10,r11,r12,r13,r14,r15
    push %\r
    .endr
    call green_tea_timer        # may switch tasks, and return here much later
    .irp r, r15,r14,r13,r12,r11,r10,r9,r8,rbp,rdi,rsi,rdx,rcx,rbx,rax
    pop %\r
    .endr
    iretq                       # restores rflags, and with it IF: interrupts back on

    # Needs neither an EOI nor anything else.
    .globl green_tea_spurious_entry
green_tea_spurious_entry:
    iretq

    .section .rodata, "a"
    .balign 8
    .globl green_tea_exception_stubs
green_tea_exception_stubs:
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    .quad green_tea_exception_\n
    .endr
"#,
    options(att_syntax)
);

unsafe extern "C" {
    static green_tea_exception_stubs: [u64; 32];
    fn green_tea_timer_entry();
    fn green_tea_spurious_entry();
}

/// The GDT, the TSS and the IDT: what the CPU needs to take an interrupt in long mode.
mod traps {
    use super::{SPURIOUS_VECTOR, TIMER_VECTOR, outb};
    use core::arch::asm;

    /// The boot GDT's segments at the same selectors (so the loaded ones stay valid), plus
    /// the TSS at 0x18 (a system descriptor, two entries long).
    static mut GDT: [u64; 5] = [0, 0x00af9a000000ffff, 0x00cf92000000ffff, 0, 0];
    const CODE: u16 = 0x08;
    const TSS_SELECTOR: u16 = 0x18;

    /// In long mode the TSS holds no task state, only stack pointers. `ist[0]` (IST1) is
    /// where exceptions run.
    #[repr(C, packed)]
    struct Tss {
        _reserved0: u32,
        rsp: [u64; 3],
        _reserved1: u64,
        ist: [u64; 7],
        _reserved2: u64,
        _reserved3: u16,
        iomap_base: u16,
    }

    const _: () = assert!(size_of::<Tss>() == 104);

    static mut TSS: Tss = Tss {
        _reserved0: 0,
        rsp: [0; 3],
        _reserved1: 0,
        ist: [0; 7],
        _reserved2: 0,
        _reserved3: 0,
        iomap_base: size_of::<Tss>() as u16, // past the end: no I/O permission bitmap
    };

    #[repr(C, align(16))]
    struct FaultStack([u8; 16384]);
    static mut FAULT_STACK: FaultStack = FaultStack([0; 16384]);

    #[derive(Clone, Copy)]
    #[repr(C)]
    struct Gate {
        offset_low: u16,
        selector: u16,
        ist: u8,
        attributes: u8,
        offset_mid: u16,
        offset_high: u32,
        _reserved: u32,
    }

    impl Gate {
        const MISSING: Gate = Gate::new(0, 0, 0);

        /// An interrupt gate (one that turns interrupts off on the way in), for ring 0.
        const fn new(handler: u64, ist: u8, attributes: u8) -> Gate {
            Gate {
                offset_low: handler as u16,
                selector: CODE,
                ist,
                attributes,
                offset_mid: (handler >> 16) as u16,
                offset_high: (handler >> 32) as u32,
                _reserved: 0,
            }
        }

        fn to(handler: u64, ist: u8) -> Gate {
            const PRESENT_INTERRUPT_GATE: u8 = 0x8e;
            Gate::new(handler, ist, PRESENT_INTERRUPT_GATE)
        }
    }

    /// A vector without a gate raises a #NP (vector 11), whose error code names it.
    static mut IDT: [Gate; 256] = [Gate::MISSING; 256];

    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }

    pub fn init() {
        // Only ever touched here, with interrupts off: no other references exist.
        unsafe {
            let (gdt, tss, idt) = (&raw mut GDT, &raw mut TSS, &raw mut IDT);
            (*tss).ist[0] = (&raw const FAULT_STACK as u64) + size_of::<FaultStack>() as u64;

            let base = tss as u64;
            let limit = size_of::<Tss>() as u64 - 1;
            const PRESENT_AVAILABLE_TSS: u64 = 0x89;
            (*gdt)[3] = (limit & 0xffff)
                | (base & 0xff_ffff) << 16
                | PRESENT_AVAILABLE_TSS << 40
                | (limit >> 16 & 0xf) << 48
                | (base >> 24 & 0xff) << 56;
            (*gdt)[4] = base >> 32;
            let pointer = Pointer { limit: size_of::<[u64; 5]>() as u16 - 1, base: gdt as u64 };
            asm!("lgdt [{}]", in(reg) &raw const pointer, options(readonly, nostack));
            asm!("ltr {:x}", in(reg) TSS_SELECTOR, options(nostack));

            for (vector, &stub) in super::green_tea_exception_stubs.iter().enumerate() {
                (*idt)[vector] = Gate::to(stub, 1);
            }
            (*idt)[TIMER_VECTOR] = Gate::to(super::green_tea_timer_entry as *const () as u64, 0);
            (*idt)[SPURIOUS_VECTOR] =
                Gate::to(super::green_tea_spurious_entry as *const () as u64, 0);
            let pointer = Pointer { limit: size_of::<[Gate; 256]>() as u16 - 1, base: idt as u64 };
            asm!("lidt [{}]", in(reg) &raw const pointer, options(readonly, nostack));

            // The 8259 PIC, which the firmware left delivering IRQ 0-15 at vectors 8-15,
            // where they'd look like exceptions: move them to 0x20..0x30, and mask them all.
            for (port, byte) in [
                (0x20, 0x11), (0xa0, 0x11), // ICW1: initialize, expect ICW4
                (0x21, 0x20), (0xa1, 0x28), // ICW2: vector bases
                (0x21, 0x04), (0xa1, 0x02), // ICW3: the second PIC hangs off IRQ 2
                (0x21, 0x01), (0xa1, 0x01), // ICW4: 8086 mode
                (0x21, 0xff), (0xa1, 0xff), // mask every IRQ
            ] {
                outb(port, byte);
            }
        }
    }
}

/// Called by an exception stub, on the fault stack.
#[unsafe(no_mangle)]
extern "C" fn green_tea_fault(frame: &ExceptionFrame) -> ! {
    let cr2: usize; // a page fault's address
    unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)) };
    if frame.vector == PAGE_FAULT && stack::guard_slot(cr2).is_some() {
        stack::overflowed(cr2);
    }
    let ExceptionFrame { vector, error, rip, rsp, .. } = *frame;
    panic!("unexpected exception {vector} (error {error:#x}): rip {rip:#x}, rsp {rsp:#x}, cr2 {cr2:#x}");
}

// ---- The local APIC timer --------------------------------------------------------------

const LAPIC: usize = 0xfee0_0000;
const LAPIC_EOI: usize = 0xb0;
const LAPIC_SPURIOUS: usize = 0xf0;
const LAPIC_LVT_TIMER: usize = 0x320;
const LAPIC_INITIAL_COUNT: usize = 0x380;
const LAPIC_CURRENT_COUNT: usize = 0x390;
const LAPIC_DIVIDE: usize = 0x3e0;
const LVT_MASKED: u32 = 1 << 16;

fn lapic_read(register: usize) -> u32 {
    unsafe { read_volatile((LAPIC + register) as *const u32) }
}

fn lapic_write(register: usize, value: u32) {
    unsafe { write_volatile((LAPIC + register) as *mut u32, value) }
}

/// The function `init_timer` installed, run on every timer tick.
static ON_TICK: Global<Option<fn()>> = Global::new(None);
/// The deadline `set_deadline` armed.
static DEADLINE: Global<u64> = Global::new(u64::MAX);
/// The APIC timer's rate, measured by `init_timer`.
static APIC_TICKS_PER_MS: Global<u64> = Global::new(0);

/// Called by `green_tea_timer_entry`, with interrupts off (it's an interrupt gate).
#[unsafe(no_mangle)]
extern "C" fn green_tea_timer() {
    // Acknowledge first: on_tick may switch tasks and not come back for a long time.
    lapic_write(LAPIC_EOI, 0);
    // The APIC can't take back an interrupt it has raised, so a tick can be stale: one
    // that fired while interrupts were off, before a later `set_deadline`. It can also be
    // a little early, from converting the deadline to APIC ticks. Either way, wait for the
    // deadline that's armed now.
    let deadline = DEADLINE.get();
    if Q35::ticks() < deadline {
        Q35::set_deadline(deadline);
        return;
    }
    DEADLINE.set(u64::MAX); // one tick per deadline
    if let Some(on_tick) = ON_TICK.get() {
        on_tick();
    }
}

impl TimerInterrupt for Q35 {
    /// Turn the local APIC on, and measure its timer against the HPET.
    fn init_timer(on_tick: fn()) {
        ON_TICK.set(Some(on_tick));
        const APIC_ENABLE: u32 = 1 << 8;
        lapic_write(LAPIC_SPURIOUS, APIC_ENABLE | SPURIOUS_VECTOR as u32);
        lapic_write(LAPIC_DIVIDE, 0b0011); // count at the APIC's clock / 16

        const MEASURE_MS: u64 = 10;
        lapic_write(LAPIC_LVT_TIMER, LVT_MASKED | TIMER_VECTOR as u32);
        lapic_write(LAPIC_INITIAL_COUNT, u32::MAX);
        crate::delay_ms(MEASURE_MS);
        let counted = u32::MAX - lapic_read(LAPIC_CURRENT_COUNT);
        APIC_TICKS_PER_MS.set((u64::from(counted) / MEASURE_MS).max(1));

        lapic_write(LAPIC_INITIAL_COUNT, 0); // stopped
        lapic_write(LAPIC_LVT_TIMER, TIMER_VECTOR as u32); // one-shot, unmasked
    }

    /// Start the APIC timer's one-shot count for the time left until `deadline`.
    fn set_deadline(deadline: u64) {
        DEADLINE.set(deadline);
        if deadline == u64::MAX {
            lapic_write(LAPIC_INITIAL_COUNT, 0); // stops it
            return;
        }
        let wait = u128::from(deadline.saturating_sub(Self::ticks()));
        let count = wait * u128::from(APIC_TICKS_PER_MS.get()) / u128::from(Self::ticks_per_ms());
        lapic_write(LAPIC_INITIAL_COUNT, count.clamp(1, u128::from(u32::MAX)) as u32);
    }

    fn interrupts_on() {
        unsafe { asm!("sti", options(nomem, nostack)) };
    }

    fn interrupts_off() -> bool {
        let rflags: u64;
        unsafe { asm!("pushfq", "pop {}", "cli", out(reg) rflags, options(nomem)) };
        rflags & RFLAGS_IF != 0
    }
}

// ---- Paging ---------------------------------------------------------------------------

/// `_start` identity-maps the first 4 GiB with 2 MiB pages. The stack pool is one of them;
/// this replaces it with a table of 4 KiB pages in which the guard pages are missing.
mod paging {
    use crate::stack;
    use core::arch::asm;

    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    const MIB2: usize = 2 << 20;

    #[repr(C, align(4096))]
    struct Table([u64; 512]);

    static mut STACKS_4K: Table = Table([0; 512]);

    unsafe extern "C" {
        /// `_start`'s four page directories, back to back: entry `i` maps `i` × 2 MiB.
        static mut boot_pds: [u64; 2048];
    }

    pub fn enable_guard_pages() {
        let stacks = stack::region_start();
        assert!(stacks.is_multiple_of(MIB2) && stacks < 2048 * MIB2);
        unsafe {
            // Only ever touched here, at boot: no other references exist.
            let (pool, pds) = (&raw mut STACKS_4K, &raw mut boot_pds);
            for (i, entry) in (*pool).0.iter_mut().enumerate() {
                *entry = (stacks + i * stack::PAGE) as u64 | PRESENT | WRITABLE;
            }
            for guard in stack::guard_pages() {
                (*pool).0[(guard - stacks) / stack::PAGE] = 0; // not present: any access faults
            }
            (*pds)[stacks / MIB2] = pool as u64 | PRESENT | WRITABLE;
            // Reloading cr3 flushes the TLB, and with it the old 2 MiB mapping.
            asm!("mov {0}, cr3", "mov cr3, {0}", out(reg) _, options(nostack));
        }
    }
}
