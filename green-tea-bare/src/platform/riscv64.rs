//! QEMU `virt`, riscv64. OpenSBI (`-bios default`) jumps to `_start` in S-mode at
//! 0x80200000 with a0 = hart id, a1 = device tree.
//!
//! Timer interrupts: the timer hardware (the CLINT's mtime/mtimecmp) belongs to M-mode,
//! so S-mode asks OpenSBI to set a deadline (the SBI TIME extension), and gets a
//! supervisor timer interrupt when it passes. The trap lands in `trap_entry` below, which
//! saves every register on the interrupted stack before calling Rust.

use super::{Platform, TimerInterrupt};
use crate::Global;
use crate::fw_cfg::Mmio;
use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};

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

    fn init() {}

    /// The 16550 UART. QEMU needs no setup: wait for room, write the byte.
    fn putc(byte: u8) {
        const BASE: usize = 0x1000_0000;
        const LSR_THR_EMPTY: u8 = 1 << 5;
        unsafe {
            while read_volatile((BASE + 5) as *const u8) & LSR_THR_EMPTY == 0 {}
            write_volatile(BASE as *mut u8, byte);
        }
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
    # Interrupts only ever arrive in a task (the scheduler runs with them off), so this is
    # a task's stack. The psABI keeps sp 16-byte aligned at every instruction, so the
    # frame is aligned too.
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
    .option pop
"#
);

unsafe extern "C" {
    fn green_tea_trap_entry();
}

/// The function `init_timer` installed, run on every supervisor timer interrupt.
static ON_TICK: Global<Option<fn()>> = Global::new(None);

/// Called by `trap_entry` with interrupts off (the trap cleared sstatus.SIE).
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
    let stval: usize;
    unsafe { asm!("csrr {}, stval", out(reg) stval) };
    panic!(
        "unexpected trap: scause {scause:#x}, sepc {:#x}, stval {stval:#x}, ra {:#x}",
        frame.sepc, frame.x[1]
    );
}

impl TimerInterrupt for RiscvVirt {
    fn init_timer(on_tick: fn()) {
        ON_TICK.set(Some(on_tick));
        Self::set_deadline(u64::MAX);
        unsafe {
            asm!("csrw stvec, {}", in(reg) green_tea_trap_entry as *const () as usize); // direct mode
            asm!("csrs sie, {}", in(reg) SIE_STIE);
        }
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
