//! QEMU `virt`, aarch64. With no firmware, QEMU loads the ELF and starts `_start` at EL1
//! with the MMU off. That makes every data access a Device access, where unaligned
//! accesses fault; the aarch64-unknown-none target compiles with +strict-align, so the
//! compiler never makes any.
//!
//! Other CPUs: with `-smp 4` the machine has four, and QEMU starts the kernel on CPU 0 and
//! keeps the rest off. `start_cpu` turns one on with PSCI's `CPU_ON`, a call that QEMU
//! itself answers when there is no firmware. A CPU's number is the low byte of its
//! `MPIDR_EL1`.
//!
//! Timer interrupts: the virtual timer of the ARM Generic Timer (the counter `ticks`
//! reads) raises PPI 11, interrupt 27, which reaches the CPU through the GICv2 interrupt
//! controller as an IRQ. The exception vectors below save every register on the
//! interrupted stack before calling Rust.

use super::{Platform, Qemu, TimerInterrupt, VirtioBus};
use crate::{Framebuffer, Global};
use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};
use green_tea_fw_cfg::Mmio;
use green_tea_pl011::Pl011;
use green_tea_virtio::{self as virtio, AnyTransport};

pub struct AarchVirt;

global_asm!(
    r#"
    .section .text.entry, "ax"
    .globl _start
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

    // Where a CPU turned on by `start_cpu` begins: at EL1 with the MMU off and interrupts
    // masked, x0 = what `start_cpu` passed, which is the top of its stack.
    .globl green_tea_secondary_entry
green_tea_secondary_entry:
    mrs x1, cpacr_el1           // FP/SIMD, as above
    orr x1, x1, #(3 << 20)
    msr cpacr_el1, x1
    isb
    mov sp, x0
    mrs x0, mpidr_el1
    and x0, x0, #0xff
    bl green_tea_secondary      // doesn't return
4:  wfi
    b 4b
"#
);

unsafe extern "C" {
    fn green_tea_secondary_entry();
}

/// A started CPU's first Rust: the exception vectors, so that a fault there is reported,
/// then the code every CPU shares.
#[unsafe(no_mangle)]
extern "C" fn green_tea_secondary(cpu: usize) -> ! {
    unsafe { asm!("msr vbar_el1, {}", "isb", in(reg) &raw const green_tea_vectors) };
    crate::cpus::enter(cpu)
}

impl Platform for AarchVirt {
    const NAME: &'static str = "aarch64";
    const GUARD_PAGES: bool = false;

    /// The serial port, and the exception vectors, so that a fault on this CPU is reported.
    fn init() {
        unsafe { asm!("msr vbar_el1, {}", "isb", in(reg) &raw const green_tea_vectors) };
        Pl011(0x0900_0000).enable();
    }

    /// A PL011 UART.
    fn putc(byte: u8) {
        Pl011(0x0900_0000).putc(byte);
    }

    fn getc() -> Option<u8> {
        Pl011(0x0900_0000).getc()
    }

    /// The generic timer's virtual count.
    fn ticks() -> u64 {
        let t: u64;
        unsafe { asm!("mrs {}, cntvct_el0", out(reg) t) };
        t
    }

    fn ticks_per_ms() -> u64 {
        let freq: u64;
        unsafe { asm!("mrs {}, cntfrq_el0", out(reg) freq) };
        freq / 1000
    }

    fn halt() -> ! {
        loop {
            unsafe { asm!("wfi") };
        }
    }

    fn cpu() -> usize {
        let mpidr: usize;
        unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr) };
        mpidr & 0xff
    }

    /// PSCI `CPU_ON` (function 0xc400_0003): the CPU to turn on, where it starts, and a
    /// value it finds in x0. The call is an `hvc`: with no hypervisor and no firmware in
    /// the machine, QEMU takes it.
    fn start_cpu(cpu: usize, stack_top: usize) -> Result<(), &'static str> {
        let error: i64;
        unsafe {
            asm!(
                "hvc #0",
                inlateout("x0") 0xc400_0003u64 => error,
                inlateout("x1") cpu => _,
                inlateout("x2") green_tea_secondary_entry as *const () as usize => _,
                inlateout("x3") stack_top => _,
            );
        }
        match error {
            0 => Ok(()),
            -2 => Err("no such CPU"), // INVALID_PARAMETERS
            _ => Err("PSCI refused to turn the CPU on"),
        }
    }

    fn display() -> Result<Framebuffer, &'static str> {
        Self::ramfb_display()
    }

    fn test_name(name: &mut [u8]) -> usize {
        Self::fw_cfg_test_name(name)
    }
}

impl Qemu for AarchVirt {
    type FwCfg = Mmio;
    const FW_CFG: Mmio = Mmio(0x0902_0000);
}

impl VirtioBus for AarchVirt {
    /// 32 virtio-mmio slots, 0x200 bytes apart.
    fn for_each_virtio(mut f: impl FnMut(virtio::Device)) -> Result<(), &'static str> {
        for slot in 0..32 {
            if let Some((id, transport)) = virtio::mmio::probe(0x0a00_0000 + slot * 0x200)? {
                f(virtio::Device { id, transport: AnyTransport::Mmio(transport) });
            }
        }
        Ok(())
    }
}

// ---- Exceptions -----------------------------------------------------------------------

/// What the vectors push on the interrupted stack: x0–x30, the exception return state
/// (ELR, SPSR: another task's exception would overwrite them), the FP control/status
/// registers, and all of q0–q31. Every register, not just the caller-saved ones, since the
/// handler may switch tasks; and the q registers in full, because only the low halves of
/// v8–v15 are callee-saved.
#[repr(C)]
struct Frame {
    x: [u64; 31], // 0..248
    elr: u64,     // 248: where the interrupted code resumes
    spsr: u64,    // 256: its PSTATE, including the interrupt mask
    fpcr: u64,    // 264
    fpsr: u64,    // 272
    _pad: u64,    // 280: q0 must be 16-byte aligned
    q: [u128; 32], // 288..800
}

const _: () = assert!(size_of::<Frame>() == 800); // the offsets the vectors use

global_asm!(
    r#"
    // 16 vectors of 0x80 bytes each: {{sync, IRQ, FIQ, SError}} for {{current EL with SP_EL0,
    // current EL with SP_ELx, lower EL in AArch64, lower EL in AArch32}}. Each saves x0 and
    // x1, puts its index in x0, and joins the common path. Index 5 (current EL, SP_ELx,
    // IRQ) is the timer; anything else is a fault, and panics.
    .section .text.vectors, "ax"
    .balign 2048
    .globl green_tea_vectors
green_tea_vectors:
    .irp kind, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    .balign 0x80
    sub sp, sp, #800
    stp x0, x1, [sp, #0]
    mov x0, #\kind
    b green_tea_exception_entry
    .endr

green_tea_exception_entry:
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x1, elr_el1
    mrs x2, spsr_el1
    stp x1, x2, [sp, #248]
    mrs x1, fpcr
    mrs x2, fpsr
    stp x1, x2, [sp, #264]
    stp q0, q1, [sp, #288]
    stp q2, q3, [sp, #320]
    stp q4, q5, [sp, #352]
    stp q6, q7, [sp, #384]
    stp q8, q9, [sp, #416]
    stp q10, q11, [sp, #448]
    stp q12, q13, [sp, #480]
    stp q14, q15, [sp, #512]
    stp q16, q17, [sp, #544]
    stp q18, q19, [sp, #576]
    stp q20, q21, [sp, #608]
    stp q22, q23, [sp, #640]
    stp q24, q25, [sp, #672]
    stp q26, q27, [sp, #704]
    stp q28, q29, [sp, #736]
    stp q30, q31, [sp, #768]

    mov x1, sp
    bl green_tea_exception      // may switch tasks, and return here much later

    ldp q0, q1, [sp, #288]
    ldp q2, q3, [sp, #320]
    ldp q4, q5, [sp, #352]
    ldp q6, q7, [sp, #384]
    ldp q8, q9, [sp, #416]
    ldp q10, q11, [sp, #448]
    ldp q12, q13, [sp, #480]
    ldp q14, q15, [sp, #512]
    ldp q16, q17, [sp, #544]
    ldp q18, q19, [sp, #576]
    ldp q20, q21, [sp, #608]
    ldp q22, q23, [sp, #640]
    ldp q24, q25, [sp, #672]
    ldp q26, q27, [sp, #704]
    ldp q28, q29, [sp, #736]
    ldp q30, q31, [sp, #768]
    ldp x1, x2, [sp, #264]
    msr fpcr, x1
    msr fpsr, x2
    ldp x1, x2, [sp, #248]
    msr elr_el1, x1
    msr spsr_el1, x2            // I = 0: eret turns interrupts back on
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #800
    eret
"#
);

unsafe extern "C" {
    static green_tea_vectors: u8;
}

const VECTOR_IRQ: u64 = 5; // current EL, SP_ELx, IRQ

/// GICv2 on `virt`: the distributor (routing, enables) and the CPU interface (acknowledge,
/// end of interrupt).
const GICD: usize = 0x0800_0000;
const GICC: usize = 0x0801_0000;
const GICD_CTLR: usize = GICD;
const GICD_ISENABLER0: usize = GICD + 0x100;
const GICD_IPRIORITYR: usize = GICD + 0x400;
const GICC_CTLR: usize = GICC;
const GICC_PMR: usize = GICC + 0x04;
const GICC_IAR: usize = GICC + 0x0c;
const GICC_EOIR: usize = GICC + 0x10;
const SPURIOUS: u32 = 1023;

/// The virtual timer's interrupt: PPI 11, and PPIs start at 16.
const TIMER_IRQ: u32 = 27;

const CNTV_CTL_ENABLE: u64 = 1;
const DAIF_I: u64 = 1 << 7;

fn gic_write(register: usize, value: u32) {
    unsafe { write_volatile(register as *mut u32, value) };
}

/// The function `init_timer` installed, run on every timer interrupt.
static ON_TICK: Global<Option<fn()>> = Global::new(None);

/// Called by the vectors with interrupts off (taking an exception masks them).
#[unsafe(no_mangle)]
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
    let (esr, far): (u64, u64);
    unsafe {
        asm!("mrs {}, esr_el1", out(reg) esr);
        asm!("mrs {}, far_el1", out(reg) far);
    }
    panic!(
        "unexpected exception: vector {kind}, esr {esr:#x}, far {far:#x}, elr {:#x}, lr {:#x}",
        frame.elr, frame.x[30]
    );
}

impl TimerInterrupt for AarchVirt {
    fn init_timer(on_tick: fn()) {
        Self::without_interrupts(|| ON_TICK.set(Some(on_tick)));
        Self::set_deadline(u64::MAX);
        unsafe {
            // The priority byte for interrupt 27 (anything below the mask passes), then
            // enable it, and turn on the distributor and this CPU's interface.
            write_volatile((GICD_IPRIORITYR + TIMER_IRQ as usize) as *mut u8, 0x80);
        }
        gic_write(GICD_ISENABLER0, 1 << TIMER_IRQ);
        gic_write(GICD_CTLR, 1);
        gic_write(GICC_PMR, 0xff);
        gic_write(GICC_CTLR, 1);
    }

    /// The virtual timer fires when the virtual count reaches CNTV_CVAL, and keeps its
    /// line high until CVAL moves past the count again.
    fn set_deadline(deadline: u64) {
        unsafe {
            asm!("msr cntv_cval_el0, {}", in(reg) deadline);
            asm!("msr cntv_ctl_el0, {}", "isb", in(reg) CNTV_CTL_ENABLE);
        }
    }

    fn interrupts_on() {
        unsafe { asm!("msr daifclr, #2") };
    }

    fn interrupts_off() -> bool {
        let daif: u64;
        unsafe { asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif) };
        daif & DAIF_I == 0
    }
}
