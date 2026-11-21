//! A polled xHCI (USB 3) host controller driver, with enough USB to use a keyboard plugged
//! into one of the controller's own ports. No interrupts: [`Xhci::poll`] reads the event
//! ring. No hubs, and no hot-plugging: the keyboard must be attached at boot.
//!
//! Bringing a device up takes the steps the xHCI specification lays out: reset the port,
//! ask for a slot, give the device an address, read its descriptors, configure it, and
//! tell the controller about the endpoint to poll. Everything the controller reads or
//! writes lives in an [`Arena`], whose addresses must be physical ones.

#![no_std]

pub mod descriptors;
pub mod hid;
pub mod trb;

use core::ptr::{read_volatile, write_bytes, write_volatile};
use core::sync::atomic::{Ordering, fence};
use descriptors::{BootKeyboard, default_max_packet, endpoint_interval, find_boot_keyboard};
use trb::{EventRing, Ring, Trb};

/// PCI class code of an xHCI controller: serial bus, USB, xHCI (in the top three bytes of
/// configuration space's dword at offset 8).
pub const PCI_CLASS: u32 = 0x000c_0330;

// Capability registers.
const HCSPARAMS1: usize = 0x04;
const HCSPARAMS2: usize = 0x08;
const HCCPARAMS1: usize = 0x10;
const DBOFF: usize = 0x14;
const RTSOFF: usize = 0x18;
// Operational registers.
const USBCMD: usize = 0x00;
const USBSTS: usize = 0x04;
const PAGESIZE: usize = 0x08;
const CRCR: usize = 0x18;
const DCBAAP: usize = 0x30;
const CONFIG: usize = 0x38;
const PORTS: usize = 0x400;
// Interrupter 0, in the runtime registers.
const ERSTSZ: usize = 0x28;
const ERSTBA: usize = 0x30;
const ERDP: usize = 0x38;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const STS_HALTED: u32 = 1 << 0;
const STS_NOT_READY: u32 = 1 << 11;
const ERDP_BUSY: u64 = 1 << 3;

const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_POWER: u32 = 1 << 9;
const PORT_RESET_CHANGE: u32 = 1 << 21;
/// All the bits that clear when 1 is written to them.
const PORT_CHANGES: u32 = 0x00fe_0000;
/// The bits that keep their value when written back. The rest are status, or clear on 1.
const PORT_PRESERVE: u32 = 0x0e00_c200;

const LEGACY_CAPABILITY: u32 = 1;
const LEGACY_BIOS_OWNED: u32 = 1 << 16;
const LEGACY_OS_OWNED: u32 = 1 << 24;
const LEGACY_SMI_ENABLES: u32 = 0x0000_e011;
const LEGACY_SMI_EVENTS: u32 = 0xe000_0000;

const MAX_SLOTS: u32 = 16;
const COMMAND_TRBS: usize = 64;
const EVENT_TRBS: usize = 256;
const TRANSFER_TRBS: usize = 32;
const PAGE: usize = 4096;
const WAIT_MS: u64 = 1000;

const CLEAR_FEATURE: u8 = 1;
const GET_DESCRIPTOR: u8 = 6;
const SET_CONFIGURATION: u8 = 9;
const SET_IDLE: u8 = 0x0a;
const SET_PROTOCOL: u8 = 0x0b;
const DEVICE_TO_HOST: u8 = 0x80;
const CLASS_TO_INTERFACE: u8 = 0x21;
const TO_ENDPOINT: u8 = 0x02;
const ENDPOINT_HALT: u16 = 0;
const DESCRIPTOR_DEVICE: u16 = 1 << 8;
const DESCRIPTOR_CONFIGURATION: u16 = 2 << 8;
const REPORT: usize = 8;
/// Errors in a row before we stop trying to restart the keyboard's endpoint.
const MAX_RECOVERIES: u8 = 3;

/// Enough for the most scratchpad pages a controller can ask for (1023, or 4 MiB), with
/// room for its rings and for the devices on its ports.
pub const ARENA_SIZE: usize = 6 << 20;

/// Zeroed memory for the controller, handed out in order and never returned.
pub struct Arena {
    next: usize,
    end: usize,
}

impl Arena {
    /// # Safety
    /// The `len` bytes at `start` must be unused, identity-mapped RAM, for good.
    pub unsafe fn new(start: *mut u8, len: usize) -> Arena {
        Arena { next: start as usize, end: start as usize + len }
    }

    /// `size` zeroed bytes aligned to `align`, within one 64 KiB page, as rings must be.
    fn alloc(&mut self, size: usize, align: usize) -> Result<usize, &'static str> {
        let mut at = self.next.next_multiple_of(align);
        if at / 0x10000 != (at + size - 1) / 0x10000 {
            at = (at + 1).next_multiple_of(0x10000);
        }
        if size > 0x10000 || at + size > self.end {
            return Err("out of memory for the USB controller");
        }
        self.next = at + size;
        // SAFETY: inside the arena, and handed out once.
        unsafe { write_bytes(at as *mut u8, 0, size) };
        Ok(at)
    }

    /// A point to come back to with [`Arena::rewind`].
    pub fn mark(&self) -> usize {
        self.next
    }

    /// Reuse everything handed out since `mark`.
    ///
    /// # Safety
    /// No controller may still be using that memory.
    pub unsafe fn rewind(&mut self, mark: usize) {
        self.next = mark;
    }
}

/// How many scratchpad pages the controller asks for: HCSPARAMS2 has the count's high five
/// bits at 21 and its low five at 27.
fn scratchpad_count(params2: u32) -> usize {
    ((params2 >> 21 & 0x1f) << 5 | params2 >> 27) as usize
}

/// The array of `count` zeroed pages the controller keeps its own state in.
fn alloc_scratchpads(arena: &mut Arena, count: usize) -> Result<usize, &'static str> {
    let array = arena.alloc(count * 8, 64)?;
    for i in 0..count {
        let page = arena.alloc(PAGE, PAGE)?;
        // SAFETY: the array has `count` entries, in the arena.
        unsafe { write_volatile((array as *mut u64).add(i), page as u64) };
    }
    Ok(array)
}

fn read32(address: usize) -> u32 {
    // SAFETY: the caller's address is a register of the mapped controller, or arena memory.
    unsafe { read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    // SAFETY: as for `read32`.
    unsafe { write_volatile(address as *mut u32, value) }
}

/// A 64-bit register, low half first, for controllers without 64-bit access.
fn write64(address: usize, value: u64) {
    write32(address, value as u32);
    write32(address + 4, (value >> 32) as u32);
}

/// What the keyboard needs after it is configured.
struct Keyboard {
    slot: u8,
    endpoint: u8,
    /// The endpoint's USB address, which a request to the device names it by.
    address: u8,
    ep0: Ring,
    failures: u8,
    ring: Ring,
    report: usize,
    decoder: hid::Decoder,
    typed: [u8; 6],
    head: usize,
    count: usize,
}

/// A running controller.
pub struct Xhci {
    operational: usize,
    runtime: usize,
    doorbells: usize,
    ports: u8,
    context_size: usize,
    dcbaa: usize,
    commands: Ring,
    events: EventRing,
    delay_ms: fn(u64),
    keyboard: Option<Keyboard>,
}

impl Xhci {
    /// Take the controller from the firmware, reset it and start it.
    ///
    /// # Safety
    /// `base` must be the controller's mapped registers, with its PCI function enabled for
    /// memory and DMA, and nothing else may be using it.
    pub unsafe fn new(base: usize, arena: &mut Arena, delay_ms: fn(u64)) -> Result<Xhci, &'static str> {
        let params1 = read32(base + HCSPARAMS1);
        let params2 = read32(base + HCSPARAMS2);
        let hcc = read32(base + HCCPARAMS1);
        let operational = base + (read32(base) & 0xff) as usize;
        let runtime = base + (read32(base + RTSOFF) & !0x1f) as usize;
        let doorbells = base + (read32(base + DBOFF) & !0x3) as usize;
        if read32(operational + PAGESIZE) & 1 == 0 {
            return Err("the USB controller doesn't support 4 KiB pages");
        }
        take_from_firmware(base, (hcc >> 16) as usize, delay_ms);

        write32(operational + USBCMD, read32(operational + USBCMD) & !CMD_RUN);
        wait(delay_ms, || read32(operational + USBSTS) & STS_HALTED != 0).ok_or("the USB controller didn't halt")?;
        write32(operational + USBCMD, CMD_RESET);
        wait(delay_ms, || {
            read32(operational + USBCMD) & CMD_RESET == 0 && read32(operational + USBSTS) & STS_NOT_READY == 0
        })
        .ok_or("the USB controller didn't reset")?;

        let slots = (params1 & 0xff).min(MAX_SLOTS);
        write32(operational + CONFIG, slots);
        let dcbaa = arena.alloc((slots as usize + 1) * 8, 64)?;
        let scratchpads = scratchpad_count(params2);
        if scratchpads > 0 {
            let array = alloc_scratchpads(arena, scratchpads)?;
            // SAFETY: entry 0 of the DCBAA, in the arena, is the scratchpad array.
            unsafe { write_volatile(dcbaa as *mut u64, array as u64) };
        }
        write64(operational + DCBAAP, dcbaa as u64);

        let command_trbs = arena.alloc(COMMAND_TRBS * size_of::<Trb>(), 64)?;
        // SAFETY: zeroed arena memory of this many TRBs.
        let commands = unsafe { Ring::new(command_trbs as *mut Trb, COMMAND_TRBS) };
        write64(operational + CRCR, commands.dequeue_pointer());

        let event_trbs = arena.alloc(EVENT_TRBS * size_of::<Trb>(), 64)?;
        let segment_table = arena.alloc(16, 64)?;
        // SAFETY: a table of one entry: the segment's address, then its size.
        unsafe {
            write_volatile(segment_table as *mut u64, event_trbs as u64);
            write_volatile((segment_table + 8) as *mut u32, EVENT_TRBS as u32);
        }
        // SAFETY: zeroed arena memory of this many TRBs.
        let events = unsafe { EventRing::new(event_trbs as *const Trb, EVENT_TRBS) };
        write32(runtime + ERSTSZ, 1);
        write64(runtime + ERDP, events.dequeue_pointer());
        write64(runtime + ERSTBA, segment_table as u64);

        let ports = (params1 >> 24) as u8;
        for port in 0..ports as usize {
            let at = operational + PORTS + 0x10 * port;
            let status = read32(at);
            if status & PORT_POWER == 0 {
                write32(at, status & PORT_PRESERVE | PORT_POWER);
            }
        }
        fence(Ordering::SeqCst);
        write32(operational + USBCMD, CMD_RUN);
        wait(delay_ms, || read32(operational + USBSTS) & STS_HALTED == 0).ok_or("the USB controller didn't start")?;
        delay_ms(200); // for what's plugged in to connect

        let context_size = if hcc & (1 << 2) != 0 { 64 } else { 32 };
        Ok(Xhci {
            operational,
            runtime,
            doorbells,
            ports,
            context_size,
            dcbaa,
            commands,
            events,
            delay_ms,
            keyboard: None,
        })
    }

    /// Look for a keyboard on each connected port, and start reading the first. Returns
    /// its port.
    pub fn find_keyboard(&mut self, arena: &mut Arena) -> Option<u8> {
        for port in 1..=self.ports {
            let status = read32(self.port(port));
            if status & PORT_CONNECTED != 0 && self.reset_port(port) && self.try_port(port, arena) == Ok(true) {
                return Some(port);
            }
        }
        None
    }

    /// Halt the controller, so that it no longer touches its memory. `false` if it won't.
    pub fn stop(&self) -> bool {
        write32(self.operational + USBCMD, read32(self.operational + USBCMD) & !CMD_RUN);
        wait(self.delay_ms, || read32(self.operational + USBSTS) & STS_HALTED != 0).is_some()
    }

    /// The next character typed on the keyboard, if one has arrived.
    pub fn poll(&mut self) -> Option<u8> {
        self.keyboard.as_ref()?;
        while let Some(event) = self.next_event() {
            self.on_keyboard_event(event);
        }
        let keyboard = self.keyboard.as_mut()?;
        (keyboard.count > 0).then(|| {
            let c = keyboard.typed[keyboard.head];
            keyboard.head = (keyboard.head + 1) % keyboard.typed.len();
            keyboard.count -= 1;
            c
        })
    }

    fn on_keyboard_event(&mut self, event: Trb) {
        let Some(k) = self.keyboard.as_mut() else {
            return;
        };
        if event.kind() != trb::TRANSFER_EVENT || event.slot() != k.slot || event.endpoint() != k.endpoint {
            return;
        }
        match event.code() {
            trb::SUCCESS | trb::SHORT_PACKET => k.failures = 0,
            trb::BABBLE | trb::TRANSACTION_ERROR | trb::STALL | trb::SPLIT_TRANSACTION_ERROR => {
                return self.recover_keyboard(); // these halt the endpoint
            }
            _ => {
                // No report, and the endpoint is still running: ask for the next one.
                k.ring.push(Trb::normal(k.report as u64, REPORT as u32));
                let (slot, endpoint) = (k.slot, k.endpoint);
                return self.ring_doorbell(slot, endpoint);
            }
        }
        // SAFETY: the report buffer, which the controller has finished writing.
        let report = unsafe { read_volatile(k.report as *const [u8; REPORT]) };
        let (typed, head, count) = (&mut k.typed, k.head, &mut k.count);
        k.decoder.feed(&report, |c| {
            if *count < typed.len() {
                typed[(head + *count) % typed.len()] = c;
                *count += 1;
            }
        });
        k.ring.push(Trb::normal(k.report as u64, REPORT as u32));
        let (slot, endpoint) = (k.slot, k.endpoint);
        self.ring_doorbell(slot, endpoint);
    }

    /// Restart the keyboard's halted endpoint. After `MAX_RECOVERIES` errors in a row, or
    /// if the restart fails, the keyboard is gone, and we stop reading it.
    fn recover_keyboard(&mut self) {
        let Some(mut k) = self.keyboard.take() else { return };
        k.failures += 1;
        if k.failures <= MAX_RECOVERIES && self.restart_endpoint(&mut k).is_ok() {
            self.keyboard = Some(k);
        }
    }

    /// What the specification asks for after a halt: reset the endpoint, move it past the
    /// failed transfer, clear the halt on the device's side too, and queue a new transfer.
    fn restart_endpoint(&mut self, k: &mut Keyboard) -> Result<(), &'static str> {
        self.command(Trb::reset_endpoint(k.slot, k.endpoint))?;
        self.command(Trb::set_tr_dequeue(k.ring.enqueue_pointer(), k.slot, k.endpoint))?;
        let clear = Trb::setup(TO_ENDPOINT, CLEAR_FEATURE, ENDPOINT_HALT, u16::from(k.address), 0);
        self.control(k.slot, &mut k.ep0, clear, None)?;
        k.ring.push(Trb::normal(k.report as u64, REPORT as u32));
        self.ring_doorbell(k.slot, k.endpoint);
        Ok(())
    }

    fn port(&self, port: u8) -> usize {
        self.operational + PORTS + 0x10 * (port as usize - 1)
    }

    /// Reset a USB 2 port. A USB 3 port trains its link by itself and is already enabled.
    fn reset_port(&self, port: u8) -> bool {
        let at = self.port(port);
        let status = read32(at);
        write32(at, status & PORT_PRESERVE | status & PORT_CHANGES);
        if status & PORT_ENABLED != 0 {
            return true;
        }
        write32(at, status & PORT_PRESERVE | PORT_RESET);
        if wait(self.delay_ms, || read32(at) & PORT_RESET_CHANGE != 0).is_none() {
            return false;
        }
        let status = read32(at);
        write32(at, status & PORT_PRESERVE | status & PORT_CHANGES);
        (self.delay_ms)(20); // the recovery time USB 2 asks for
        read32(at) & PORT_ENABLED != 0
    }

    fn ring_doorbell(&self, slot: u8, target: u8) {
        fence(Ordering::SeqCst);
        write32(self.doorbells + 4 * slot as usize, u32::from(target));
    }

    fn next_event(&mut self) -> Option<Trb> {
        let event = self.events.pop()?;
        write64(self.runtime + ERDP, self.events.dequeue_pointer() | ERDP_BUSY);
        Some(event)
    }

    /// Run a command and wait for it. Returns the completion event.
    fn command(&mut self, command: Trb) -> Result<Trb, &'static str> {
        let address = self.commands.push(command);
        self.ring_doorbell(0, 0);
        for _ in 0..WAIT_MS {
            while let Some(event) = self.next_event() {
                if event.kind() == trb::COMMAND_COMPLETION && event.param == address {
                    return if event.code() == trb::SUCCESS { Ok(event) } else { Err("a USB command failed") };
                }
            }
            (self.delay_ms)(1);
        }
        Err("a USB command timed out")
    }

    /// A control transfer on endpoint 0.
    fn control(&mut self, slot: u8, ring: &mut Ring, setup: Trb, data: Option<(u64, u16)>) -> Result<(), &'static str> {
        let into_memory = setup.param & 0x80 != 0;
        ring.push(setup);
        if let Some((buffer, length)) = data {
            ring.push(Trb::data(buffer, length, into_memory));
        }
        ring.push(Trb::status_stage(data.is_some() && into_memory));
        self.ring_doorbell(slot, 1);
        for _ in 0..WAIT_MS {
            while let Some(event) = self.next_event() {
                if event.kind() == trb::TRANSFER_EVENT && event.slot() == slot && event.endpoint() == 1 {
                    return match event.code() {
                        trb::SUCCESS | trb::SHORT_PACKET => Ok(()),
                        _ => Err("a USB control transfer failed"),
                    };
                }
            }
            (self.delay_ms)(1);
        }
        Err("a USB control transfer timed out")
    }

    /// Dword `dword` of context `index` in the block of contexts at `base`.
    fn context(&self, base: usize, index: usize, dword: usize) -> usize {
        base + index * self.context_size + dword * 4
    }

    /// Bring up the device on a port. `Ok(true)` if it is a keyboard, now being read.
    fn try_port(&mut self, port: u8, arena: &mut Arena) -> Result<bool, &'static str> {
        let speed = (read32(self.port(port)) >> 10 & 0xf) as u8;
        let slot = self.command(Trb::enable_slot())?.slot();
        let output = arena.alloc(32 * self.context_size, 64)?;
        let input = arena.alloc(33 * self.context_size, 64)?;
        let trbs = arena.alloc(TRANSFER_TRBS * size_of::<Trb>(), 64)?;
        let buffer = arena.alloc(512, 64)?;
        // SAFETY: zeroed arena memory of this many TRBs.
        let mut ep0 = unsafe { Ring::new(trbs as *mut Trb, TRANSFER_TRBS) };
        // SAFETY: `slot` came from the controller, and the DCBAA has an entry for each.
        unsafe { write_volatile((self.dcbaa as *mut u64).add(slot as usize), output as u64) };

        // Input control context: add the slot context and endpoint 0.
        write32(self.context(input, 0, 1), 0b11);
        write32(self.context(input, 1, 0), u32::from(speed) << 20 | 1 << 27); // one endpoint
        write32(self.context(input, 1, 1), u32::from(port) << 16);
        self.write_ep0(input, default_max_packet(speed), ep0.dequeue_pointer());
        self.command(Trb::with_context(trb::ADDRESS_DEVICE, input as u64, slot))?;

        // The first 8 bytes of the device descriptor say how big endpoint 0's packets are.
        let request = Trb::setup(DEVICE_TO_HOST, GET_DESCRIPTOR, DESCRIPTOR_DEVICE, 0, 8);
        self.control(slot, &mut ep0, request, Some((buffer as u64, 8)))?;
        // SAFETY: the buffer, which the controller has finished writing.
        let b_max_packet = unsafe { read_volatile((buffer + 7) as *const u8) };
        let max_packet = if speed >= 4 { 1 << b_max_packet.min(9) } else { u16::from(b_max_packet) };
        if max_packet != default_max_packet(speed) && max_packet >= 8 {
            write32(self.context(input, 0, 1), 0b10);
            self.write_ep0(input, max_packet, ep0.dequeue_pointer());
            self.command(Trb::with_context(trb::EVALUATE_CONTEXT, input as u64, slot))?;
        }

        let request = Trb::setup(DEVICE_TO_HOST, GET_DESCRIPTOR, DESCRIPTOR_CONFIGURATION, 0, 9);
        self.control(slot, &mut ep0, request, Some((buffer as u64, 9)))?;
        // SAFETY: as above.
        let total = unsafe { read_volatile((buffer + 2) as *const u16) }.min(512);
        let request = Trb::setup(DEVICE_TO_HOST, GET_DESCRIPTOR, DESCRIPTOR_CONFIGURATION, 0, total);
        self.control(slot, &mut ep0, request, Some((buffer as u64, total)))?;
        // SAFETY: the controller has written `total` bytes, which fit in the 512.
        let config = unsafe { core::slice::from_raw_parts(buffer as *const u8, total as usize) };
        let Some(found) = find_boot_keyboard(config) else { return Ok(false) };

        let request = Trb::setup(0, SET_CONFIGURATION, u16::from(found.configuration), 0, 0);
        self.control(slot, &mut ep0, request, None)?;
        self.control(
            slot,
            &mut ep0,
            Trb::setup(CLASS_TO_INTERFACE, SET_PROTOCOL, 0, u16::from(found.interface), 0),
            None,
        )?;
        // Report only on a change. A keyboard may refuse, and that's harmless here.
        self.request_idle(slot, &mut ep0, u16::from(found.interface));
        self.start_keyboard(slot, speed, (output, input), found, ep0, arena)?;
        Ok(true)
    }

    /// Ask the keyboard to report only on a change. One that refuses stalls the request,
    /// which is harmless but halts endpoint 0, so we restart it for the requests to come.
    fn request_idle(&mut self, slot: u8, ep0: &mut Ring, interface: u16) {
        const EP0: u8 = 1; // its device context index
        if self.control(slot, ep0, Trb::setup(CLASS_TO_INTERFACE, SET_IDLE, 0, interface, 0), None).is_err() {
            let dequeue = ep0.enqueue_pointer();
            let _ = self
                .command(Trb::reset_endpoint(slot, EP0))
                .and_then(|_| self.command(Trb::set_tr_dequeue(dequeue, slot, EP0)));
        }
    }

    fn write_ep0(&self, input: usize, max_packet: u16, dequeue: u64) {
        const CONTROL: u32 = 4;
        write32(self.context(input, 2, 1), 3 << 1 | CONTROL << 3 | u32::from(max_packet) << 16);
        write64(self.context(input, 2, 2), dequeue);
        write32(self.context(input, 2, 4), 8); // average TRB length
    }

    fn start_keyboard(
        &mut self,
        slot: u8,
        speed: u8,
        (output, input): (usize, usize),
        found: BootKeyboard,
        ep0: Ring,
        arena: &mut Arena,
    ) -> Result<(), &'static str> {
        const INTERRUPT_IN: u32 = 7;
        let endpoint = found.endpoint * 2 + 1; // its device context index
        let trbs = arena.alloc(TRANSFER_TRBS * size_of::<Trb>(), 64)?;
        let report = arena.alloc(REPORT, 64)?;
        // SAFETY: zeroed arena memory of this many TRBs.
        let mut ring = unsafe { Ring::new(trbs as *mut Trb, TRANSFER_TRBS) };

        for dword in 0..self.context_size / 4 {
            write32(self.context(input, 0, dword), 0);
        }
        write32(self.context(input, 0, 1), 1 | 1 << endpoint);
        // The slot context as the controller has it, with more endpoints.
        let first = read32(self.context(output, 0, 0));
        write32(self.context(input, 1, 0), first & !(0x1f << 27) | u32::from(endpoint) << 27);
        write32(self.context(input, 1, 1), read32(self.context(output, 0, 1)));
        write32(self.context(input, 1, 2), read32(self.context(output, 0, 2)));
        write32(self.context(input, 1, 3), 0);

        let index = endpoint as usize + 1;
        let packet = u32::from(found.max_packet);
        write32(self.context(input, index, 0), u32::from(endpoint_interval(speed, found.interval)) << 16);
        write32(self.context(input, index, 1), 3 << 1 | INTERRUPT_IN << 3 | packet << 16);
        write64(self.context(input, index, 2), ring.dequeue_pointer());
        write32(self.context(input, index, 4), packet | packet << 16); // average TRB length, payload
        self.command(Trb::with_context(trb::CONFIGURE_ENDPOINT, input as u64, slot))?;

        ring.push(Trb::normal(report as u64, REPORT as u32));
        self.ring_doorbell(slot, endpoint);
        self.keyboard = Some(Keyboard {
            slot,
            endpoint,
            address: found.endpoint | 0x80,
            ep0,
            failures: 0,
            ring,
            report,
            decoder: hid::Decoder::new(),
            typed: [0; 6],
            head: 0,
            count: 0,
        });
        Ok(())
    }
}

/// Ask the firmware to let go of the controller, if it holds it, and stop it from taking
/// system management interrupts for USB. `offset` is the first extended capability, in
/// dwords from `base`.
fn take_from_firmware(base: usize, mut offset: usize, delay_ms: fn(u64)) {
    while offset != 0 {
        let at = base + offset * 4;
        let capability = read32(at);
        if capability & 0xff == LEGACY_CAPABILITY {
            write32(at, capability | LEGACY_OS_OWNED);
            if wait(delay_ms, || read32(at) & LEGACY_BIOS_OWNED == 0).is_none() {
                // The firmware didn't answer. Take the controller regardless, as Linux does.
                write32(at, read32(at) & !LEGACY_BIOS_OWNED | LEGACY_OS_OWNED);
            }
            let control = read32(at + 4);
            write32(at + 4, control & !LEGACY_SMI_ENABLES | LEGACY_SMI_EVENTS);
            return;
        }
        let next = (capability >> 8 & 0xff) as usize;
        if next == 0 {
            return;
        }
        offset += next;
    }
}

/// Wait up to a second for `done`.
fn wait(delay_ms: fn(u64), mut done: impl FnMut() -> bool) -> Option<()> {
    for _ in 0..WAIT_MS {
        if done() {
            return Some(());
        }
        delay_ms(1);
    }
    done().then_some(())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::sync::atomic::AtomicUsize;
    use std::vec;
    use std::vec::Vec;

    #[test]
    fn the_arena_aligns_and_zeroes() {
        let mut memory = vec![0xffu8; 8192];
        // SAFETY: the vector outlives the arena.
        let mut arena = unsafe { Arena::new(memory.as_mut_ptr(), memory.len()) };
        let a = arena.alloc(10, 64).unwrap();
        let b = arena.alloc(10, 64).unwrap();
        assert!(a.is_multiple_of(64) && b.is_multiple_of(64) && b >= a + 10);
        // SAFETY: inside the vector.
        assert_eq!(unsafe { *(a as *const u8) }, 0);
    }

    #[test]
    fn the_arena_refuses_past_its_end() {
        let mut memory = vec![0u8; 128];
        // SAFETY: the vector outlives the arena.
        let mut arena = unsafe { Arena::new(memory.as_mut_ptr(), memory.len()) };
        assert!(arena.alloc(4096, 1).is_err());
    }

    #[test]
    fn nothing_from_the_arena_crosses_64_kib() {
        let mut memory = vec![0u8; 0x30000];
        let start = memory.as_mut_ptr() as usize;
        // SAFETY: the vector outlives the arena.
        let mut arena = unsafe { Arena::new(memory.as_mut_ptr(), memory.len()) };
        let near = (start + 32).next_multiple_of(0x10000) - 16 - start;
        arena.alloc(near, 1).unwrap();
        let a = arena.alloc(64, 16).unwrap();
        assert_eq!(a / 0x10000, (a + 63) / 0x10000);
        assert!(a.is_multiple_of(0x10000), "moved to the next 64 KiB, and no further");
        assert!(a - start <= near + 0x10000);
    }

    #[test]
    fn the_arena_holds_the_most_scratchpads_a_controller_can_ask_for() {
        assert_eq!(scratchpad_count(0x1f << 21 | 0x1f << 27), 1023);
        let mut memory = vec![0u8; ARENA_SIZE];
        // SAFETY: the vector outlives the arena.
        let mut arena = unsafe { Arena::new(memory.as_mut_ptr(), memory.len()) };
        let array = alloc_scratchpads(&mut arena, 1023).unwrap();
        // SAFETY: the array has 1023 entries.
        let pages = unsafe { core::slice::from_raw_parts(array as *const u64, 1023) };
        assert!(pages.iter().all(|p| p.is_multiple_of(PAGE as u64)));
        assert!(arena.alloc(0x10000, 64).is_ok(), "room is left for the rings");
    }

    #[test]
    fn a_rewound_arena_hands_the_memory_out_again() {
        let mut memory = vec![0xffu8; 8192];
        // SAFETY: the vector outlives the arena.
        let mut arena = unsafe { Arena::new(memory.as_mut_ptr(), memory.len()) };
        let mark = arena.mark();
        let a = arena.alloc(4096, 64).unwrap();
        // SAFETY: nothing uses it.
        unsafe { (a as *mut u8).write(7) };
        unsafe { arena.rewind(mark) };
        let b = arena.alloc(4096, 64).unwrap();
        assert_eq!(a, b);
        // SAFETY: inside the vector.
        assert_eq!(unsafe { *(b as *const u8) }, 0, "zeroed again");
    }

    #[test]
    fn firmware_that_never_lets_go_loses_the_controller() {
        let mut registers = vec![0u32; 8];
        registers[4] = LEGACY_CAPABILITY | LEGACY_BIOS_OWNED;
        registers[5] = LEGACY_SMI_ENABLES;
        take_from_firmware(registers.as_mut_ptr() as usize, 4, |_| {});
        assert_eq!(registers[4] & LEGACY_BIOS_OWNED, 0);
        assert_ne!(registers[4] & LEGACY_OS_OWNED, 0);
        assert_eq!(registers[5] & LEGACY_SMI_ENABLES, 0);
    }

    #[test]
    fn firmware_that_lets_go_is_waited_for() {
        static CAPABILITY: AtomicUsize = AtomicUsize::new(0);
        let mut registers = vec![0u32; 8];
        registers[4] = LEGACY_CAPABILITY | LEGACY_BIOS_OWNED;
        CAPABILITY.store(registers[4..].as_mut_ptr() as usize, Ordering::SeqCst);
        take_from_firmware(registers.as_mut_ptr() as usize, 4, |_| {
            let at = CAPABILITY.load(Ordering::SeqCst);
            assert_ne!(read32(at) & LEGACY_OS_OWNED, 0, "we asked before waiting");
            write32(at, read32(at) & !LEGACY_BIOS_OWNED);
        });
        assert_eq!(registers[4] & LEGACY_BIOS_OWNED, 0);
    }

    /// A controller that completes every command and control transfer it finds, run from
    /// the driver's delay callback, and that records the commands in order.
    mod fake {
        use super::*;
        use std::sync::Mutex;
        use std::vec::Vec;

        pub struct State {
            pub commands: usize,
            pub command_at: usize,
            pub ep0: usize,
            pub ep0_at: usize,
            pub events: usize,
            pub event_at: usize,
            pub seen: Vec<Trb>,
            pub fail_commands: bool,
            /// Stall a SET_IDLE request, as a keyboard that doesn't support it may.
            pub stall_idle: bool,
            pub last_request: u8,
        }

        pub static STATE: Mutex<Option<State>> = Mutex::new(None);

        fn emit(s: &mut State, trb: Trb) {
            // SAFETY: the test's event ring, which is long enough for the test.
            unsafe { write_volatile((s.events as *mut Trb).add(s.event_at), Trb { control: trb.control | 1, ..trb }) };
            s.event_at += 1;
        }

        pub fn run(_ms: u64) {
            let mut guard = STATE.lock().unwrap();
            let s = guard.as_mut().unwrap();
            loop {
                let at = s.commands + s.command_at * size_of::<Trb>();
                // SAFETY: the test's command ring.
                let trb = unsafe { read_volatile(at as *const Trb) };
                if trb.control & 1 == 0 {
                    break;
                }
                s.command_at += 1;
                s.seen.push(trb);
                let code = if s.fail_commands { 5 } else { trb::SUCCESS };
                let control = trb::COMMAND_COMPLETION << 10 | trb.control & 0xff00_0000;
                emit(s, Trb { param: at as u64, status: code << 24, control });
            }
            loop {
                // SAFETY: the test's endpoint 0 ring.
                let trb = unsafe { read_volatile((s.ep0 as *const Trb).add(s.ep0_at)) };
                if trb.control & 1 == 0 {
                    break;
                }
                s.ep0_at += 1;
                s.seen.push(trb);
                if trb.kind() == trb::SETUP {
                    s.last_request = (trb.param >> 8) as u8;
                }
                if trb.kind() == trb::STATUS {
                    let control = trb::TRANSFER_EVENT << 10 | 1 << 16 | 1 << 24;
                    let code = if s.stall_idle && s.last_request == SET_IDLE { trb::STALL } else { trb::SUCCESS };
                    emit(s, Trb { param: 0, status: code << 24, control });
                }
            }
        }
    }

    struct Rig {
        xhci: Xhci,
        interrupt: Vec<Trb>,
        _registers: Vec<u64>,
        _rings: [Vec<Trb>; 3],
        _report: Vec<u8>,
    }

    fn rig(fail_commands: bool) -> Rig {
        let mut registers = vec![0u64; 1024];
        let mut commands = vec![Trb::default(); 32];
        let mut events = vec![Trb::default(); 64];
        let mut ep0 = vec![Trb::default(); 32];
        let mut interrupt = vec![Trb::default(); 8];
        let report = vec![0u8; REPORT];
        let base = registers.as_mut_ptr() as usize;
        *fake::STATE.lock().unwrap() = Some(fake::State {
            commands: commands.as_mut_ptr() as usize,
            command_at: 0,
            ep0: ep0.as_mut_ptr() as usize,
            ep0_at: 0,
            events: events.as_mut_ptr() as usize,
            event_at: 0,
            seen: Vec::new(),
            fail_commands,
            stall_idle: false,
            last_request: 0,
        });
        // SAFETY: each ring is zeroed and kept alive in the rig.
        let xhci = unsafe {
            let mut ring = Ring::new(interrupt.as_mut_ptr(), interrupt.len());
            ring.push(Trb::normal(report.as_ptr() as u64, REPORT as u32));
            Xhci {
                operational: base,
                runtime: base,
                doorbells: base + 0x1000,
                ports: 1,
                context_size: 32,
                dcbaa: base,
                commands: Ring::new(commands.as_mut_ptr(), commands.len()),
                events: EventRing::new(events.as_ptr(), events.len()),
                delay_ms: fake::run,
                keyboard: Some(Keyboard {
                    slot: 1,
                    endpoint: 3,
                    address: 0x81,
                    ep0: Ring::new(ep0.as_mut_ptr(), ep0.len()),
                    failures: 0,
                    ring,
                    report: report.as_ptr() as usize,
                    decoder: hid::Decoder::new(),
                    typed: [0; 6],
                    head: 0,
                    count: 0,
                }),
            }
        };
        Rig { xhci, interrupt, _registers: registers, _rings: [commands, events, ep0], _report: report }
    }

    fn failed_transfer() -> Trb {
        Trb { param: 0, status: trb::STALL << 24, control: trb::TRANSFER_EVENT << 10 | 3 << 16 | 1 << 24 | 1 }
    }

    /// `fake::STATE` is one controller, so the tests that use it take turns.
    static RIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_halted_keyboard_endpoint_is_restarted() {
        let _turn = RIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut rig = rig(false);
        let after_failed = rig.interrupt.as_ptr() as u64 + size_of::<Trb>() as u64;
        rig.xhci.on_keyboard_event(failed_transfer());
        let seen = fake::STATE.lock().unwrap().as_ref().unwrap().seen.clone();
        let kinds: Vec<u32> = seen.iter().map(Trb::kind).collect();
        assert_eq!(kinds, [trb::RESET_ENDPOINT, trb::SET_TR_DEQUEUE, trb::SETUP, trb::STATUS]);
        assert_eq!(seen[0].endpoint(), 3);
        assert_eq!(seen[1].param, after_failed | 1, "past the failed transfer");
        assert_eq!(seen[2].param, Trb::setup(0x02, 1, 0, 0x81, 0).param, "CLEAR_FEATURE(ENDPOINT_HALT)");
        assert_eq!(rig.interrupt[1].kind(), trb::NORMAL, "a new transfer is queued");
        assert!(rig.xhci.keyboard.is_some());
    }

    #[test]
    fn a_refused_set_idle_leaves_endpoint_0_usable() {
        let _turn = RIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut rig = rig(false);
        fake::STATE.lock().unwrap().as_mut().unwrap().stall_idle = true;
        let mut k = rig.xhci.keyboard.take().unwrap();
        rig.xhci.request_idle(1, &mut k.ep0, 0);
        let seen = fake::STATE.lock().unwrap().as_ref().unwrap().seen.clone();
        let kinds: Vec<u32> = seen.iter().map(Trb::kind).collect();
        assert_eq!(kinds, [trb::SETUP, trb::STATUS, trb::RESET_ENDPOINT, trb::SET_TR_DEQUEUE]);
        assert_eq!((seen[2].endpoint(), seen[3].endpoint()), (1, 1), "endpoint 0's context index");
        assert_eq!(seen[3].param, k.ep0.enqueue_pointer(), "past the refused request");
    }

    #[test]
    fn a_keyboard_that_keeps_failing_is_given_up_on() {
        let _turn = RIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut rig = rig(false);
        for _ in 0..MAX_RECOVERIES {
            rig.xhci.on_keyboard_event(failed_transfer());
            assert!(rig.xhci.keyboard.is_some());
        }
        rig.xhci.on_keyboard_event(failed_transfer());
        assert!(rig.xhci.keyboard.is_none());
    }

    #[test]
    fn a_failed_restart_gives_the_keyboard_up() {
        let _turn = RIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut rig = rig(true);
        rig.xhci.on_keyboard_event(failed_transfer());
        assert!(rig.xhci.keyboard.is_none());
    }

    #[test]
    fn port_write_bits_never_clear_a_change_or_disable_the_port() {
        assert_eq!(PORT_PRESERVE & PORT_ENABLED, 0);
        assert_eq!(PORT_PRESERVE & PORT_CHANGES, 0);
        assert_ne!(PORT_PRESERVE & PORT_POWER, 0);
    }
}
