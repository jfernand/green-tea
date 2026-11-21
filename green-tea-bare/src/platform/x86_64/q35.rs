//! QEMU `q35`. QEMU boots the ELF through PVH: it finds the entry point in a Xen ELF note
//! and jumps there in 32-bit protected mode, paging off, which is how `_start` expects to
//! be entered. fw_cfg says how much RAM and how many CPUs there are, and a CPU's number is
//! its local APIC's id.

use super::{apic_id, paging, traps};
use crate::platform::{Platform, Qemu, VirtioBus};
use crate::{Framebuffer, pages};
use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};
use green_tea_fw_cfg::IoPorts;
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
    .section .text, "ax"
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

    /// Start the HPET's main counter, which `ticks` reads. Catch exceptions (so a fault
    /// panics with a report instead of triple-faulting into a reset), and leave out the
    /// guard pages and the heap's pages.
    fn init() {
        unsafe { write_volatile(HPET_CONFIG, read_volatile(HPET_CONFIG) | 1) };
        traps::init();
        paging::enable_guard_pages();
        paging::unmap_heap();
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

    /// The local APIC's id: each CPU reads its own APIC at the same address.
    fn cpu() -> usize {
        apic_id() as usize
    }

    /// INIT, then STARTUP, sent to the CPU's local APIC from ours. Neither message is
    /// answered, so whether the CPU exists has to come from elsewhere: fw_cfg's item 5 is
    /// the number of CPUs QEMU was started with.
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

    /// RAM starts at 0, and fw_cfg's item 3 is its size in bytes: what QEMU's `-m` said. A
    /// write past it faults on nothing here, and is lost.
    fn ram_end() -> Option<usize> {
        use green_tea_fw_cfg::FwCfgPort;
        IoPorts.select(0x03);
        Some(u64::from_le_bytes([0; 8].map(|_| IoPorts.read_u8())) as usize)
    }

    fn display() -> Result<Framebuffer, &'static str> {
        Self::ramfb_display()
    }

    fn test_name(name: &mut [u8]) -> usize {
        Self::fw_cfg_test_name(name)
    }
}

impl Qemu for Q35 {
    type FwCfg = IoPorts;
    const FW_CFG: IoPorts = IoPorts;
}

impl VirtioBus for Q35 {
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
