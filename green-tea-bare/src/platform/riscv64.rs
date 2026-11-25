//! QEMU `virt`, riscv64. OpenSBI (`-bios default`) jumps to `_start` in S-mode at
//! 0x80200000 with a0 = hart id, a1 = device tree.
//!
//! Other harts: with `-smp 4` the machine has four, and OpenSBI starts the kernel on one
//! of them (not always hart 0) and keeps the rest stopped. `start_cpu` asks it to start
//! one, with the SBI HSM extension's `hart_start`. Each hart keeps its CPU number in `tp`,
//! which compiled code leaves alone on this target.
//!
//! Timer interrupts: the timer hardware (the CLINT's mtime/mtimecmp) belongs to M-mode,
//! so S-mode asks OpenSBI to set a deadline (the SBI TIME extension), and gets a
//! supervisor timer interrupt when it passes. The trap lands in `trap_entry` below, which
//! saves every register on the interrupted stack before calling Rust.
//!
//! Guard pages: `init` turns on Sv39 paging with an identity map, in which the guard page
//! under each task stack is missing. A task that overflows takes a page fault there,
//! which `trap_entry` handles on a stack of its own (the task's is the one that's full).
//!
//! Demand paging: the same map leaves out every page of the heap. A fault there maps the
//! page (`crate::pages::fault`), and `trap_entry` returns to the instruction that faulted.

use super::{Platform, Qemu, TimerInterrupt, VirtioBus};
use crate::{Framebuffer, Global};
use crate::{pages, stack};
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

    # This hart is CPU 0. Remember which hart it is: the others are started by hart id.
    mv tp, zero
    la t0, green_tea_boot_hart
    sd a0, 0(t0)

    call kmain
3:  wfi
    j 3b

    # Where a hart started by `start_cpu` begins: in S-mode with paging and interrupts
    # off, a0 = its hart id, and a1 = what `start_cpu` passed, which is the address of a
    # word just under the top of its stack, holding its CPU number.
    .globl green_tea_secondary_entry
green_tea_secondary_entry:
    mv sp, a1
    ld tp, 0(sp)
    li t0, 1 << 13              # the FPU, as above
    csrs sstatus, t0
    mv a0, tp
    call green_tea_secondary    # doesn't return
4:  wfi
    j 4b
"#
);

/// The hart `_start` ran on.
#[unsafe(no_mangle)]
static mut green_tea_boot_hart: usize = 0;

unsafe extern "C" {
    fn green_tea_secondary_entry();
}

/// A started hart's first Rust: the trap entry and the page tables of the boot hart, then
/// the code every CPU shares.
#[unsafe(no_mangle)]
extern "C" fn green_tea_secondary(cpu: usize) -> ! {
    unsafe { asm!("csrw stvec, {}", in(reg) green_tea_trap_entry as *const () as usize) };
    paging::join();
    crate::cpus::enter(cpu)
}

impl Platform for RiscvVirt {
    const NAME: &'static str = "riscv64";
    const GUARD_PAGES: bool = true;
    const DEMAND_PAGING: bool = true;

    fn set_mapped(page: usize, mapped: bool) -> bool {
        paging::set_mapped(page, mapped)
    }

    fn is_mapped(page: usize) -> bool {
        paging::is_mapped(page)
    }

    fn walk(addr: usize) -> [Option<pages::Step>; 4] {
        paging::walk(addr)
    }

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

    fn cpu() -> usize {
        let cpu: usize;
        unsafe { asm!("mv {}, tp", out(reg) cpu) };
        cpu
    }

    /// SBI HSM extension ("HSM"), function 0: `sbi_hart_start(hartid, start_addr, opaque)`.
    /// CPUs 1, 2, ... are the harts other than the boot hart, in order of hart id.
    fn start_cpu(cpu: usize, stack_top: usize) -> Result<(), &'static str> {
        // SAFETY: written once, by `_start`, before any Rust ran.
        let boot_hart = unsafe { green_tea_boot_hart };
        let hart = if cpu <= boot_hart { cpu - 1 } else { cpu };
        let word = stack_top - 16; // keeps the stack 16-byte aligned
        // SAFETY: the stack is the caller's to give, and nothing runs on it yet.
        unsafe { (word as *mut usize).write(cpu) };
        let error: isize;
        unsafe {
            asm!(
                "ecall",
                inlateout("a0") hart => error,
                inlateout("a1") green_tea_secondary_entry as *const () as usize => _,
                in("a2") word,
                in("a6") 0,
                in("a7") 0x48_534d,
            );
        }
        match error {
            0 => Ok(()),
            -3 => Err("no such hart"), // SBI_ERR_INVALID_PARAM
            _ => Err("the firmware refused to start the hart"),
        }
    }

    fn display() -> Result<Framebuffer, &'static str> {
        Self::ramfb_display()
    }

    fn test_name(name: &mut [u8]) -> usize {
        Self::fw_cfg_test_name(name)
    }
}

impl Qemu for RiscvVirt {
    type FwCfg = Mmio;
    const FW_CFG: Mmio = Mmio(0x1010_0000);
}

impl VirtioBus for RiscvVirt {
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
/// are unused. For a fault the frame is on the fault stack, and `x[2]` holds the faulting
/// code's sp. Every register is saved, not just the caller-saved ones that an ordinary
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
    # points into a guard page, and pushing a frame would fault again, forever. So faults
    # get a stack of their own. scause's top bit says which: set (negative) for an
    # interrupt.
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

9:  # A fault. Save the same frame as for an interrupt, on the fault stack, with the
    # faulting code's sp in the slot an interrupt leaves unused. The handler returns only
    # if it has dealt with the fault (a page of the heap, now mapped): then everything is
    # put back and sret runs the faulting instruction again. One fault at a time on each
    # CPU: a fault in the handler would start again at the top of its fault stack.
    csrr t0, sscratch
    csrw sscratch, sp
    # This CPU's fault stack: the top of stack number tp. No register is free here, so tp
    # is used for the sum, and put back.
    la sp, green_tea_fault_stacks
    addi tp, tp, 1
    slli tp, tp, 14             # 16 KiB each
    add sp, sp, tp
    srli tp, tp, 14
    addi tp, tp, -1
    addi sp, sp, -544
    .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    sd x\n, \n*8(sp)
    .endr
    csrr t0, sscratch
    sd t0, 16(sp)
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
    call green_tea_fault

    ld t0, 272(sp)
    fscsr t0
    ld t0, 264(sp)
    csrw sstatus, t0
    ld t0, 256(sp)
    csrw sepc, t0
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fld f\n, 288+\n*8(sp)
    .endr
    .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    ld x\n, \n*8(sp)
    .endr
    ld sp, 16(sp)
    sret
    .option pop

    .section .bss.fault_stack, "aw", @nobits
    .balign 16
green_tea_fault_stacks:
    .skip {fault_stacks}        # one for each of cpus::MAX
"#,
    fault_stacks = const crate::cpus::MAX * 16384,
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

/// Called by `trap_entry` for an exception, on the fault stack. Returns if the fault was
/// for a page of the heap that wasn't mapped, and now is.
#[unsafe(no_mangle)]
extern "C" fn green_tea_fault(frame: &TrapFrame) {
    let (scause, stval): (usize, usize);
    unsafe {
        asm!("csrr {}, scause", out(reg) scause);
        asm!("csrr {}, stval", out(reg) stval);
    }
    let page_fault = scause == SCAUSE_LOAD_PAGE_FAULT || scause == SCAUSE_STORE_PAGE_FAULT;
    if page_fault && pages::fault(stval) {
        return;
    }
    if page_fault && stack::guard_slot(stval).is_some() {
        stack::overflowed(stval);
    }
    let (sepc, sp, ra) = (frame.sepc, frame.x[2], frame.x[1]);
    panic!("unexpected exception: scause {scause:#x}, stval {stval:#x}, sepc {sepc:#x}, sp {sp:#x}, ra {ra:#x}");
}

impl TimerInterrupt for RiscvVirt {
    fn init_timer(on_tick: fn()) {
        Self::without_interrupts(|| ON_TICK.set(Some(on_tick)));
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
/// adds is the holes: the guard pages, which stay missing, and the pages of the heap,
/// which are missing until they are first touched.
mod paging {
    use crate::Global;
    use crate::{pages, stack};
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
    /// 4 KiB per entry, for the 2 MiB pages the heap is in. The heap is 4 MiB and starts
    /// wherever .stack ends, so it is in three of them, and shares the first and the last
    /// with other sections; or in two, if it starts on a 2 MiB boundary, and then the third
    /// table is not used: the next 2 MiB page is the stack pool's.
    static mut HEAP_4K: [Table; HEAP_TABLES] = [const { Table([0; 512]) }; HEAP_TABLES];
    const HEAP_TABLES: usize = 3;
    /// The address that `HEAP_4K[0]` starts at.
    static HEAP_BASE: Global<usize> = Global::new(0);

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

            // The heap: a table for each 2 MiB page it is in, mapping what else is there
            // as before and leaving every page of the heap out.
            let heap = pages::heap_range();
            let base = heap.start & !(MIB2 - 1);
            assert!(heap.start >= RAM && heap.end <= base + HEAP_TABLES * MIB2 && heap.end <= stacks);
            HEAP_BASE.set(base);
            let tables = &raw mut HEAP_4K;
            for (t, table) in (*tables).iter_mut().enumerate().take((heap.end - base).div_ceil(MIB2)) {
                for (i, entry) in table.0.iter_mut().enumerate() {
                    let page = base + t * MIB2 + i * pages::PAGE;
                    *entry = if heap.contains(&page) { 0 } else { leaf(page, MEMORY) };
                }
                (*ram).0[(base - RAM) / MIB2 + t] = branch(table);
            }

            switch_on();
        }
    }


    /// What the CPU does on a TLB miss, in software: start at the table satp names, and
    /// at each of the three levels take nine more bits of the address as an index. An
    /// entry with any of R, W and X set is a leaf, and ends the walk.
    pub fn walk(addr: usize) -> [Option<pages::Step>; 4] {
        use pages::PointsTo::{Nothing, Page, Table};
        let satp: usize;
        unsafe { asm!("csrr {}, satp", out(reg) satp) };
        let mut table = (satp & ((1 << 44) - 1)) << 12;
        let mut steps = [None; 4];
        for (level, step) in steps.iter_mut().take(3).enumerate() {
            let shift = 30 - 9 * level;
            let index = (addr >> shift) & 511;
            // SAFETY: a table is 512 entries, and the tables are mapped where they are.
            let entry = unsafe { (table as *const u64).add(index).read_volatile() };
            let target = ((entry >> 10) << 12) as usize;
            let points_to = if entry & V == 0 {
                Nothing
            } else if entry & (R | W | X) != 0 {
                Page { frame: target, size: 1 << shift }
            } else {
                Table(target)
            };
            *step = Some(pages::Step { index, entry, points_to });
            let Table(next) = points_to else { break };
            table = next;
        }
        steps
    }

    /// The entry for a page of the heap.
    fn heap_entry(page: usize) -> *mut u64 {
        let index = (page - HEAP_BASE.get()) / pages::PAGE;
        assert!(pages::heap_range().contains(&page) && index < HEAP_TABLES * 512);
        // SAFETY: in bounds, and nothing holds a reference to the tables.
        unsafe { (&raw mut HEAP_4K).cast::<u64>().add(index) }
    }

    pub fn is_mapped(page: usize) -> bool {
        unsafe { heap_entry(page).read_volatile() & V != 0 }
    }

    /// Write the entry, then `sfence.vma` for the one address. The CPU may have the old
    /// entry in its TLB, and that includes an entry that was not valid.
    pub fn set_mapped(page: usize, mapped: bool) -> bool {
        let entry = heap_entry(page);
        let was_mapped = is_mapped(page);
        unsafe {
            entry.write_volatile(if mapped { leaf(page, MEMORY) } else { 0 });
            asm!("sfence.vma {}, zero", in(reg) page);
        }
        was_mapped
    }

    /// Point this hart at the tables. `satp` is a register of each hart's own.
    unsafe fn switch_on() {
        const SV39: usize = 8 << 60;
        let satp = SV39 | (&raw const ROOT as usize >> 12);
        unsafe { asm!("sfence.vma", "csrw satp, {}", "sfence.vma", in(reg) satp) };
    }

    /// Turn paging on for a hart started after the boot hart, with the tables `enable` built.
    pub fn join() {
        // SAFETY: `enable` finished before any other hart was started, and nothing writes
        // the tables after it.
        unsafe { switch_on() };
    }
}
