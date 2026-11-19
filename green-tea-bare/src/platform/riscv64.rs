//! QEMU `virt`, riscv64. OpenSBI (`-bios default`) jumps to `_start` in S-mode at
//! 0x80200000 with a0 = hart id, a1 = device tree.
//!
//! Timer interrupts: the timer hardware (the CLINT's mtime/mtimecmp) belongs to M-mode,
//! so S-mode asks OpenSBI to set a deadline (the SBI TIME extension), and gets a
//! supervisor timer interrupt when it passes. The trap lands in `trap_entry` below, which
//! saves every register on the interrupted stack before calling Rust.
//!
//! Guard pages: `init` turns on Sv39 paging with an identity map, in which the guard page
//! under each task stack is missing. A task that overflows takes a page fault there,
//! which `trap_entry` handles on a stack of its own (the task's is the one that's full).

use super::{Platform, TimerInterrupt};
use crate::Global;
use crate::stack;
use core::arch::{asm, global_asm};
use green_tea_fw_cfg::Mmio;
use green_tea_ns16550::Ns16550Mmio;
use green_tea_virtio::{self as virtio, AnyTransport};

pub struct RiscvVirt;

global_asm!(
    r#"
    .section .text.entry, "ax"
    .globl _start
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

    call kmain
3:  wfi
    j 3b
"#
);

impl Platform for RiscvVirt {
    const NAME: &'static str = "riscv64";
    const GUARD_PAGES: bool = true;

    /// Catch traps (so a fault panics with a report instead of hanging in OpenSBI), and
    /// turn on paging.
    fn init() {
        unsafe { asm!("csrw stvec, {}", in(reg) green_tea_trap_entry as *const () as usize) }; // direct mode
        paging::enable();
    }

    /// A 16550 UART.
    fn putc(byte: u8) {
        Ns16550Mmio(0x1000_0000).putc(byte);
    }

    fn getc() -> Option<u8> {
        Ns16550Mmio(0x1000_0000).getc()
    }

    /// The `time` CSR, which ticks at 10 MHz on QEMU's virt machine.
    fn ticks() -> u64 {
        let t: u64;
        unsafe { asm!("rdtime {}", out(reg) t) };
        t
    }

    fn ticks_per_ms() -> u64 {
        10_000
    }

    fn halt() -> ! {
        loop {
            unsafe { asm!("wfi") };
        }
    }

    type FwCfg = Mmio;
    const FW_CFG: Mmio = Mmio(0x1010_0000);

    /// Eight virtio-mmio slots, 4 KiB apart.
    fn for_each_virtio(mut f: impl FnMut(virtio::Device)) -> Result<(), &'static str> {
        for slot in 0..8 {
            if let Some((id, transport)) = virtio::mmio::probe(0x1000_1000 + slot * 0x1000)? {
                f(virtio::Device { id, transport: AnyTransport::Mmio(transport) });
            }
        }
        Ok(())
    }
}

// ---- Traps ----------------------------------------------------------------------------

const SSTATUS_SIE: usize = 1 << 1; // interrupts on, in S-mode
const SIE_STIE: usize = 1 << 5; // supervisor timer interrupt enable
const SCAUSE_INTERRUPT: usize = 1 << 63;
const SCAUSE_SUPERVISOR_TIMER: usize = SCAUSE_INTERRUPT | 5;

/// What `trap_entry` pushes on the interrupted stack. `x[n]` holds register xn; x0 (zero)
/// and x2 (sp, which is recovered by popping the frame) aren't saved, so `x[0]` and `x[2]`
/// are unused. Every register is saved, not just the caller-saved ones that an ordinary
/// call would clobber: the handler may switch tasks, and the frame is also what you'd want
/// to inspect when an unexpected trap panics.
#[repr(C)]
struct TrapFrame {
    x: [u64; 32],   // 0..256
    sepc: u64,      // 256: where the interrupted code resumes
    sstatus: u64,   // 264: its SPIE/SPP, which another task's trap would overwrite
    fcsr: u64,      // 272
    _pad: u64,      // 280: keep the frame a multiple of 16 bytes
    f: [u64; 32],   // 288..544
}

const _: () = assert!(size_of::<TrapFrame>() == 544); // the offsets trap_entry uses

global_asm!(
    r#"
    .section .text, "ax"
    # The FP saves need the D extension. riscv64gc has it, but global asm doesn't always
    # get the target's features (release builds don't), so say so, as green-tea-arch does.
    .option push
    .option arch, +d
    .balign 4                   # stvec's direct mode needs a 4-byte aligned handler
    .globl green_tea_trap_entry
green_tea_trap_entry:
    # An exception (a fault), rather than an interrupt, may be a stack overflow: then sp
    # points into a guard page, and pushing a frame would fault again, forever. Faults
    # never return here, so they get a stack of their own. scause's top bit says which:
    # set (negative) for an interrupt.
    csrw sscratch, t0
    csrr t0, scause
    bgez t0, 9f
    csrr t0, sscratch

    # An interrupt. They only ever arrive in a task (the scheduler runs with them off), so
    # this is a task's stack. The psABI keeps sp 16-byte aligned at every instruction, so
    # the frame is aligned too.
    addi sp, sp, -544
    .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    sd x\n, \n*8(sp)
    .endr
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fsd f\n, 288+\n*8(sp)
    .endr
    csrr t0, sepc
    sd t0, 256(sp)
    csrr t0, sstatus
    sd t0, 264(sp)
    frcsr t0
    sd t0, 272(sp)

    mv a0, sp
    call green_tea_trap         # may switch tasks, and return here much later

    ld t0, 272(sp)
    fscsr t0
    ld t0, 264(sp)
    csrw sstatus, t0            # SPIE = 1, SPP = S: sret turns interrupts back on
    ld t0, 256(sp)
    csrw sepc, t0
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fld f\n, 288+\n*8(sp)
    .endr
    .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    ld x\n, \n*8(sp)
    .endr
    addi sp, sp, 544
    sret

9:  # A fault: report it from the fault stack, with the old sp and ra.
    mv a0, sp
    mv a1, ra
    la sp, green_tea_fault_stack_top
    call green_tea_fault        # doesn't return
    .option pop

    .section .bss.fault_stack, "aw", @nobits
    .balign 16
    .skip 16384
green_tea_fault_stack_top:
"#
);

unsafe extern "C" {
    fn green_tea_trap_entry();
}

/// The function `init_timer` installed, run on every supervisor timer interrupt.
static ON_TICK: Global<Option<fn()>> = Global::new(None);

/// Called by `trap_entry` for an interrupt, with interrupts off (the trap cleared
/// sstatus.SIE).
#[unsafe(no_mangle)]
extern "C" fn green_tea_trap(frame: &TrapFrame) {
    let scause: usize;
    unsafe { asm!("csrr {}, scause", out(reg) scause) };
    if scause == SCAUSE_SUPERVISOR_TIMER {
        match ON_TICK.get() {
            Some(on_tick) => on_tick(),
            None => RiscvVirt::set_deadline(u64::MAX), // otherwise it stays pending
        }
        return;
    }
    panic!("unexpected interrupt: scause {scause:#x}, sepc {:#x}", frame.sepc);
}

const SCAUSE_LOAD_PAGE_FAULT: usize = 13;
const SCAUSE_STORE_PAGE_FAULT: usize = 15;

/// Called by `trap_entry` for an exception, on the fault stack. `sp` and `ra` are the
/// faulting code's.
#[unsafe(no_mangle)]
extern "C" fn green_tea_fault(sp: usize, ra: usize) -> ! {
    let (scause, stval, sepc): (usize, usize, usize);
    unsafe {
        asm!("csrr {}, scause", out(reg) scause);
        asm!("csrr {}, stval", out(reg) stval);
        asm!("csrr {}, sepc", out(reg) sepc);
    }
    let page_fault = scause == SCAUSE_LOAD_PAGE_FAULT || scause == SCAUSE_STORE_PAGE_FAULT;
    if page_fault && stack::guard_slot(stval).is_some() {
        stack::overflowed(stval);
    }
    panic!("unexpected exception: scause {scause:#x}, stval {stval:#x}, sepc {sepc:#x}, sp {sp:#x}, ra {ra:#x}");
}

impl TimerInterrupt for RiscvVirt {
    fn init_timer(on_tick: fn()) {
        ON_TICK.set(Some(on_tick));
        Self::set_deadline(u64::MAX);
        unsafe { asm!("csrs sie, {}", in(reg) SIE_STIE) }; // init installed the trap entry
    }

    /// SBI TIME extension ("TIME"), function 0: `sbi_set_timer`. It also clears a pending
    /// timer interrupt.
    fn set_deadline(deadline: u64) {
        unsafe {
            asm!(
                "ecall",
                inlateout("a0") deadline => _,
                lateout("a1") _,
                in("a6") 0,
                in("a7") 0x5449_4d45,
            );
        }
    }

    fn interrupts_on() {
        unsafe { asm!("csrs sstatus, {}", in(reg) SSTATUS_SIE) };
    }

    fn interrupts_off() -> bool {
        let previous: usize;
        unsafe { asm!("csrrc {}, sstatus, {}", out(reg) previous, in(reg) SSTATUS_SIE) };
        previous & SSTATUS_SIE != 0
    }
}

// ---- Paging ---------------------------------------------------------------------------

/// Sv39: three levels of 512-entry tables, translating 39-bit virtual addresses. A leaf
/// entry at the top level maps 1 GiB, one level down 2 MiB, at the bottom 4 KiB. The map
/// is the identity, as it was with paging off, so nothing else has to change; what paging
/// adds is the holes.
mod paging {
    use crate::stack;
    use core::arch::asm;

    const V: u64 = 1 << 0; // valid
    const R: u64 = 1 << 1;
    const W: u64 = 1 << 2;
    const X: u64 = 1 << 3;
    const A: u64 = 1 << 6; // accessed and dirty: set them up front, or QEMU faults the
    const D: u64 = 1 << 7; // first access to have them set (no hardware A/D updating)
    const DEVICE: u64 = V | R | W | A | D;
    const MEMORY: u64 = V | R | W | X | A | D;

    const GIB: usize = 1 << 30;
    const MIB2: usize = 2 << 20;
    const RAM: usize = 0x8000_0000; // to 0xc000_0000: up to 1 GiB of RAM

    #[repr(C, align(4096))]
    struct Table([u64; 512]);

    static mut ROOT: Table = Table([0; 512]); // 1 GiB per entry
    static mut RAM_2M: Table = Table([0; 512]); // 2 MiB per entry, for the GiB of RAM
    static mut STACKS_4K: Table = Table([0; 512]); // 4 KiB per entry, for the stack pool

    fn leaf(address: usize, flags: u64) -> u64 {
        ((address as u64 >> 12) << 10) | flags
    }

    fn branch(table: *const Table) -> u64 {
        ((table as u64 >> 12) << 10) | V
    }

    /// Build the tables and switch them on. Physical and virtual addresses are the same
    /// throughout, so the table addresses can go straight into the entries, and the code
    /// keeps running across the switch.
    pub fn enable() {
        let stacks = stack::region_start();
        assert!(stacks.is_multiple_of(MIB2) && (RAM..RAM + GIB).contains(&stacks));
        unsafe {
            // Only ever touched here, before paging is on: no other references exist.
            let (root, ram, pool) = (&raw mut ROOT, &raw mut RAM_2M, &raw mut STACKS_4K);

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

            const SV39: usize = 8 << 60;
            let satp = SV39 | (&raw const ROOT as usize >> 12);
            asm!("sfence.vma", "csrw satp, {}", "sfence.vma", in(reg) satp);
        }
    }
}
