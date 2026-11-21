//! A physical x86_64 PC, booted by GRUB from UEFI through Multiboot2. GRUB enters `_start`
//! in 32-bit protected mode with paging off, as PVH does, and leaves a description of the
//! machine in memory. Everything QEMU's board knows by address, this one looks up:
//!
//! - RAM and the command line (which test to run) from the Multiboot2 information
//! - the screen from the framebuffer GRUB set up, wherever it is and whatever its size
//! - the CPUs from ACPI's MADT, whose APIC ids can have gaps, so a CPU's number is its
//!   position in that list and not its APIC id
//! - the clock from the TSC, measured against the HPET or, on a machine without one, the
//!   ACPI power management timer
//!
//! Output goes to the screen, and to COM1 if there is one: a laptop has none, and QEMU's
//! serial port is how the tests read the result. Input comes from either, and from a PS/2
//! keyboard. It also runs in QEMU with UEFI firmware: `cargo xtask kernel --board pc`.

use super::{apic_id, enable_x2apic_if_available, paging, set_lapic_base, traps, wake_cpu};
use crate::cpus::MAX;
use crate::platform::Platform;
use crate::{Framebuffer, pages};
use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use green_tea_acpi::{Acpi, Identity4G};
use green_tea_framebuffer::Text;
use green_tea_i8042::Keyboard;
use green_tea_io::port::{inb, inl, outb};
use green_tea_multiboot2::{BOOT_MAGIC, Info};
use green_tea_nvme::Nvme;
use green_tea_ns16550::Ns16550Port;
use green_tea_pci::Ecam;
use green_tea_xhci::{ARENA_SIZE, Arena, PCI_CLASS, Xhci};

pub struct Pc;

global_asm!(
    r#"
    # The Multiboot2 header GRUB looks for in the first 32 KiB of the file: a magic number,
    # the architecture (0: 32-bit protected mode), its length and a checksum, then tags.
    # The framebuffer tag asks for a 32-bit graphics mode of any size, and is optional.
    .section .multiboot2, "a"
    .balign 8
mb2_start:
    .long 0xe85250d6
    .long 0
    .long mb2_end - mb2_start
    .long 0x100000000 - (0xe85250d6 + (mb2_end - mb2_start))
    .short 5, 1
    .long 20
    .long 0, 0, 32
    .balign 8
    .short 0, 0
    .long 8
mb2_end:
    .section .text, "ax"
"#,
    options(att_syntax)
);

unsafe extern "C" {
    static green_tea_boot_magic: u32;
    static green_tea_boot_info: u32;
    static __heap_start: u8;
    static __kernel_end: u8;
}

const COM1: u16 = 0x3f8;
/// Where the screen's text starts when the demos' 640×480 leaves room beside or below it.
const BESIDE_DEMO: usize = 648;
const BELOW_DEMO: usize = 488;
/// Where `start_cpu`'s trampoline goes (see `AP_TRAMPOLINE`), which must be free RAM.
const TRAMPOLINE_PAGE: u64 = 0x8000;
const CALIBRATE_MS: u64 = 10;
const PM_TIMER_HZ: u64 = 3_579_545;

static HAS_SERIAL: AtomicBool = AtomicBool::new(false);
static TSC_PER_MS: AtomicU64 = AtomicU64::new(1);
static RAM_END: AtomicUsize = AtomicUsize::new(0);
static TRAMPOLINE_FREE: AtomicBool = AtomicBool::new(false);
/// Each CPU's APIC id, by CPU number. Written once by `init`, before any other CPU starts.
static APIC_IDS: [AtomicU32; MAX] = [const { AtomicU32::new(0) }; MAX];
static CPU_COUNT: AtomicUsize = AtomicUsize::new(1);

/// Held while drawing a character. Whoever finds it taken (another CPU, or an interrupt
/// that came in the middle of one) leaves the character off the screen: losing a letter is
/// better than a deadlock in `panic!`.
static TEXT_BUSY: AtomicBool = AtomicBool::new(false);
static mut TEXT: Option<Text> = None;
static mut SCREEN: Option<Framebuffer> = None;
/// The USB controller with a keyboard on it, guarded by KEYBOARD_BUSY as the PS/2 one is.
static mut USB: Option<Xhci> = None;
/// What the USB controllers read and write. In .bss, so that it is mapped from the start.
#[repr(align(4096))]
struct UsbMemory(#[allow(dead_code)] [u8; USB_MEMORY]);
const USB_MEMORY: usize = ARENA_SIZE;
static mut USB_ARENA: UsbMemory = UsbMemory([0; USB_MEMORY]);
/// What the demos draw on when GRUB found no screen we can use.
const OFFSCREEN_W: usize = 640;
const OFFSCREEN_H: usize = 480;
static mut OFFSCREEN: [u32; OFFSCREEN_W * OFFSCREEN_H] = [0; OFFSCREEN_W * OFFSCREEN_H];
/// PCI segment 0's ECAM window, once it is mapped: its base (0 for none), and its first and
/// last bus, in the low two bytes.
static ECAM_BASE: AtomicU64 = AtomicU64::new(0);
static ECAM_BUSES: AtomicU32 = AtomicU32::new(0);
/// What the NVMe controller reads and writes, for the one driver `find_nvme` makes.
static mut NVME_MEMORY: green_tea_nvme::Memory = green_tea_nvme::Memory::new();
static NVME_TAKEN: AtomicBool = AtomicBool::new(false);
static KEYBOARD_BUSY: AtomicBool = AtomicBool::new(false);
static mut KEYBOARD: Keyboard = Keyboard::new();
/// The test named on the command line.
static mut TEST: [u8; 16] = [0; 16];
static TEST_LEN: AtomicUsize = AtomicUsize::new(0);

/// Whether a UART answers at COM1, by what it does with its scratch register. If so, set
/// it to 115200 8N1: no firmware has to have done that.
fn init_serial() -> bool {
    // SAFETY: on a machine without a UART these ports read as 0xff and ignore writes.
    unsafe {
        outb(COM1 + 7, 0x5a);
        if inb(COM1 + 7) != 0x5a {
            return false;
        }
        outb(COM1 + 7, 0xa5);
        if inb(COM1 + 7) != 0xa5 {
            return false;
        }
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // the divisor latch
        outb(COM1, 1); // 115200 baud
        outb(COM1 + 1, 0);
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit
        outb(COM1 + 2, 0x07); // FIFOs on, and cleared
    }
    true
}

fn rdtsc() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: reads a counter.
    unsafe { asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack)) };
    u64::from(high) << 32 | u64::from(low)
}

/// How many TSC ticks a millisecond is, measured over `CALIBRATE_MS` against a clock whose
/// rate ACPI lets us know.
fn measure_tsc(acpi: Option<&Acpi<Identity4G>>) -> u64 {
    let acpi = acpi.unwrap_or_else(|| panic!("no ACPI tables, so no clock to measure the TSC with"));
    let (start, end) = if let Some(hpet) = acpi.hpet().filter(|&a| a < 1 << 32) {
        let hpet = hpet as usize;
        paging::map_device(hpet, 0x400).unwrap_or_else(|e| panic!("hpet: {e}"));
        // SAFETY: ACPI's address for the HPET, inside the identity map.
        unsafe {
            let config = (hpet + 0x10) as *mut u64;
            write_volatile(config, read_volatile(config) | 1);
            let period_fs = read_volatile(hpet as *const u64) >> 32;
            let counter = (hpet + 0xf0) as *const u64;
            let wait = CALIBRATE_MS * 1_000_000_000_000 / period_fs;
            let (from, start) = (read_volatile(counter), rdtsc());
            while read_volatile(counter).wrapping_sub(from) < wait {}
            (start, rdtsc())
        }
    } else if let Some(port) = acpi.pm_timer_port() {
        const MASK: u32 = 0x00ff_ffff; // 24 bits is all a timer is sure to have
        let wait = (PM_TIMER_HZ * CALIBRATE_MS / 1000) as u32;
        // SAFETY: ACPI's port for the timer; reading it has no effect.
        unsafe {
            let (from, start) = (inl(port) & MASK, rdtsc());
            while (inl(port) & MASK).wrapping_sub(from) & MASK < wait {}
            (start, rdtsc())
        }
    } else {
        panic!("ACPI lists neither an HPET nor a power management timer");
    };
    ((end - start) / CALIBRATE_MS).max(1)
}

/// Keep the screen, and start the text console on the part the demos don't draw on.
fn init_screen(info: &Info) {
    let Some(d) = info.display().filter(|d| d.xrgb) else { return };
    if let Err(e) = paging::map_high(d.addr as usize, d.pitch as usize * d.height as usize) {
        crate::println!("framebuffer: {e}");
        return;
    }
    let (w, h) = (d.width as usize, d.height as usize);
    // SAFETY: GRUB's framebuffer, of this shape, now mapped; nothing else uses that memory.
    let mut fb = unsafe { Framebuffer::new(d.addr as *mut u32, w, h, d.pitch as usize / 4) };
    fb.fill_rect(0, 0, w, h, 0); // the firmware's logo and GRUB's text are still there
    let (x, y) = if w >= 1024 {
        (BESIDE_DEMO, 0)
    } else if h >= 600 {
        (0, BELOW_DEMO)
    } else {
        (0, 0)
    };
    // SAFETY: at boot, on the only CPU, with interrupts off.
    unsafe {
        SCREEN = Some(fb);
        TEXT = Some(Text::new(fb, x, y, w - x, h - y));
    }
}

/// Find the xHCI controllers through ACPI's MCFG table, and start reading the first USB
/// keyboard on them. A machine without one still boots: USB is only a way to type.
fn init_usb(acpi: Option<&Acpi<Identity4G>>) {
    let Some((base, buses)) = acpi.and_then(|a| a.ecam()).filter(|(base, _)| *base < 1 << 32) else { return };
    let window = base as usize + ((*buses.start() as usize) << 20);
    let window_len = (*buses.end() as usize - *buses.start() as usize + 1) << 20;
    if let Err(e) = paging::map_device(window, window_len) {
        return crate::println!("usb: {e}");
    }
    ECAM_BUSES.store(u32::from(*buses.start()) << 8 | u32::from(*buses.end()), Ordering::Relaxed);
    ECAM_BASE.store(base, Ordering::Release);
    let ecam = Ecam(base as usize);
    // SAFETY: at boot, on the only CPU; nothing else uses the arena.
    let mut arena = unsafe { Arena::new((&raw mut USB_ARENA).cast::<u8>(), USB_MEMORY) };
    let mut found = None;
    ecam.for_each_function_on(buses, |function| {
        if found.is_some() || ecam.read_u32(function, 8) >> 8 != PCI_CLASS {
            return;
        }
        let Some(bar) = ecam.bar(function, 0) else { return };
        if let Err(e) = paging::map_device(bar as usize, 0x10000) {
            return crate::println!("usb: {e}");
        }
        ecam.enable(function);
        let mark = arena.mark();
        // SAFETY: the function's registers, just mapped and enabled, and used only here.
        match unsafe { Xhci::new(bar as usize, &mut arena, crate::delay_ms) } {
            Ok(mut xhci) => {
                if let Some(port) = xhci.find_keyboard(&mut arena) {
                    crate::println!("usb: keyboard on port {port}");
                    found = Some(xhci);
                } else if xhci.stop() {
                    // SAFETY: that controller is halted, and no longer uses its memory.
                    unsafe { arena.rewind(mark) };
                }
            }
            Err(e) => crate::println!("usb: {e}"),
        }
    });
    // SAFETY: at boot, on the only CPU.
    unsafe { USB = found };
}

fn init_cpus(acpi: Option<&Acpi<Identity4G>>) {
    let boot = apic_id();
    APIC_IDS[0].store(boot, Ordering::Relaxed);
    let Some(acpi) = acpi else { return };
    let mut ids = [0; MAX];
    let count = acpi.apic_ids(boot, &mut ids).max(1);
    for (slot, id) in APIC_IDS.iter().zip(&ids[..count]) {
        slot.store(*id, Ordering::Relaxed);
    }
    CPU_COUNT.store(count, Ordering::Release);
}

impl Pc {
    /// Start the first NVMe controller on the PCI bus. Resetting it is safe for what's on
    /// the disk, and nothing is written unless the caller writes. Can be called once, and
    /// only while one CPU is running, since it changes the page tables.
    pub fn find_nvme() -> Result<Option<Nvme>, &'static str> {
        if NVME_TAKEN.swap(true, Ordering::AcqRel) {
            return Err("the NVMe driver is already in use");
        }
        let base = ECAM_BASE.load(Ordering::Acquire);
        if base == 0 {
            return Ok(None);
        }
        let buses = ECAM_BUSES.load(Ordering::Relaxed);
        let ecam = Ecam(base as usize);
        let mut found = None;
        ecam.for_each_function_on((buses >> 8) as u8..=buses as u8, |function| {
            if found.is_none() && ecam.read_u32(function, 8) >> 8 == green_tea_nvme::PCI_CLASS {
                found = ecam.bar(function, 0).map(|bar| (function, bar));
            }
        });
        let Some((function, bar)) = found else { return Ok(None) };
        paging::map_device(bar as usize, green_tea_nvme::REGISTERS)?;
        ecam.enable(function);
        // SAFETY: the function's registers, just mapped and enabled. NVME_TAKEN makes this
        // the only driver for them, and the only user of NVME_MEMORY, which is in .bss.
        unsafe { Nvme::new(bar as usize, &raw mut NVME_MEMORY, crate::delay_ms) }.map(Some)
    }
}

impl Platform for Pc {
    const NAME: &'static str = "x86_64 (pc)";
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

    /// Read what GRUB and the firmware left, then set up as `Q35` does. The memory `_start`
    /// doesn't clear (the heap and the task stacks) is cleared here: unlike QEMU's, a real
    /// machine's RAM doesn't start out zero.
    fn init() {
        traps::init();
        HAS_SERIAL.store(init_serial(), Ordering::Relaxed);
        // SAFETY: `_start` stored what the loader passed, and the boot information is
        // mapped (GRUB puts it below 4 GiB) and outside the kernel.
        let (magic, address) = unsafe { (read_volatile(&raw const green_tea_boot_magic), green_tea_boot_info) };
        assert_eq!(magic, BOOT_MAGIC, "not booted by a Multiboot2 loader");
        let info = unsafe { Info::from_addr(address as usize) }.expect("bad Multiboot2 information");

        init_screen(&info);
        let test = info.arg("test").unwrap_or("").as_bytes();
        let len = test.len().min(16);
        // SAFETY: at boot, on the only CPU.
        unsafe { core::ptr::copy_nonoverlapping(test.as_ptr(), (&raw mut TEST).cast::<u8>(), len) };
        TEST_LEN.store(len, Ordering::Relaxed);

        let kernel = (&raw const __heap_start) as u64;
        RAM_END.store(info.free_end(kernel).expect("the kernel isn't in free RAM") as usize, Ordering::Relaxed);
        TRAMPOLINE_FREE.store(info.free_end(TRAMPOLINE_PAGE).is_some_and(|end| end >= TRAMPOLINE_PAGE + 4096), Ordering::Relaxed);

        let acpi = info.rsdp().and_then(|rsdp| Acpi::new(Identity4G, rsdp));
        TSC_PER_MS.store(measure_tsc(acpi.as_ref()), Ordering::Relaxed);
        if let Some(address) = acpi.as_ref().and_then(|a| a.lapic_address()) {
            set_lapic_base(address);
            paging::map_device(address as usize, 0x1000).unwrap_or_else(|e| panic!("local APIC: {e}"));
        }
        enable_x2apic_if_available();
        init_cpus(acpi.as_ref());

        // SAFETY: the linker's symbols bound the heap and the stack pool, which nothing
        // uses yet; the boot stack is below them.
        unsafe {
            let (start, end) = (&raw const __heap_start as *mut u8, &raw const __kernel_end as usize);
            core::ptr::write_bytes(start, 0, end - start as usize);
        }
        paging::enable_guard_pages();
        paging::unmap_heap();
        init_usb(acpi.as_ref());
    }

    fn putc(byte: u8) {
        if HAS_SERIAL.load(Ordering::Relaxed) {
            Ns16550Port(COM1).putc(byte);
        }
        if !TEXT_BUSY.swap(true, Ordering::Acquire) {
            // SAFETY: TEXT_BUSY is ours, and `init_screen` is done or hasn't begun.
            if let Some(text) = unsafe { (&raw mut TEXT).as_mut() }.and_then(Option::as_mut) {
                text.put(byte);
            }
            TEXT_BUSY.store(false, Ordering::Release);
        }
    }

    fn getc() -> Option<u8> {
        if HAS_SERIAL.load(Ordering::Relaxed)
            && let Some(byte) = Ns16550Port(COM1).getc()
        {
            return Some(byte);
        }
        if KEYBOARD_BUSY.swap(true, Ordering::Acquire) {
            return None;
        }
        // SAFETY: KEYBOARD_BUSY is ours.
        let usb = unsafe { (&raw mut USB).as_mut() }.and_then(Option::as_mut).and_then(Xhci::poll);
        // SAFETY: KEYBOARD_BUSY is ours.
        let byte = usb.or_else(|| unsafe { (&raw mut KEYBOARD).as_mut() }.and_then(Keyboard::getc));
        KEYBOARD_BUSY.store(false, Ordering::Release);
        byte
    }

    /// The TSC, which runs at a constant rate on any recent CPU.
    fn ticks() -> u64 {
        rdtsc()
    }

    fn ticks_per_ms() -> u64 {
        TSC_PER_MS.load(Ordering::Relaxed)
    }

    fn halt() -> ! {
        loop {
            // SAFETY: waits for an interrupt.
            unsafe { asm!("hlt") };
        }
    }

    /// This CPU's position in the MADT's list.
    fn cpu() -> usize {
        let id = apic_id();
        let count = CPU_COUNT.load(Ordering::Acquire);
        APIC_IDS[..count].iter().position(|slot| slot.load(Ordering::Relaxed) == id).unwrap_or(0)
    }

    fn start_cpu(cpu: usize, stack_top: usize) -> Result<(), &'static str> {
        if cpu >= CPU_COUNT.load(Ordering::Acquire) {
            return Err("no such CPU");
        }
        if !TRAMPOLINE_FREE.load(Ordering::Relaxed) {
            return Err("the firmware is using the page other CPUs start in");
        }
        wake_cpu(APIC_IDS[cpu].load(Ordering::Relaxed), stack_top);
        Ok(())
    }

    fn ram_end() -> Option<usize> {
        Some(RAM_END.load(Ordering::Relaxed))
    }

    fn display() -> Result<Framebuffer, &'static str> {
        // SAFETY: only `init` writes it, before this can be called.
        let Some(fb) = (unsafe { (&raw const SCREEN).read() }) else {
            // A machine can run without a screen, so the demos draw into plain memory.
            crate::println!("framebuffer: none usable, drawing to memory");
            let pixels = (&raw mut OFFSCREEN).cast::<u32>();
            // SAFETY: a static of this shape, and every call shares the one set of pixels.
            return Ok(unsafe { Framebuffer::new(pixels, OFFSCREEN_W, OFFSCREEN_H, OFFSCREEN_W) });
        };
        crate::println!("framebuffer: {}x{} XRGB8888", fb.width(), fb.height());
        Ok(fb)
    }

    fn test_name(name: &mut [u8]) -> usize {
        let len = TEST_LEN.load(Ordering::Relaxed).min(name.len());
        // SAFETY: only `init` writes it, before this can be called.
        unsafe { core::ptr::copy_nonoverlapping((&raw const TEST).cast::<u8>(), name.as_mut_ptr(), len) };
        len
    }
}
