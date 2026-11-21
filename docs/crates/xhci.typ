#import "../lib.typ": *

= green-tea-xhci <crate-xhci>

`green-tea-xhci` is a polled xHCI (USB 3) host controller driver, with enough USB to read a keyboard plugged into one of the controller's own ports. It takes the controller from the firmware, resets it, brings up the device on each connected port until it finds a HID boot keyboard, and then reads that keyboard's reports from an event ring. No interrupts are involved.

#spec(
  ("Source", [#repo("drivers/xhci/src/"), 4 files, 1,095 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `pc` board)]),
  ("Tests", [27 host tests of the rings, descriptors, reports and arena; a typed line in QEMU]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [crate root], [`Xhci` (`new`, `find_keyboard`, `poll`), `Arena`, `PCI_CLASS`],
  [`trb`], [`Trb`, the driver's `Ring`, the controller's `EventRing`, and the TRB type and completion constants],
  [`descriptors`], [`find_boot_keyboard`, `BootKeyboard`, `endpoint_interval`, `default_max_packet`],
  [`hid`], [`Decoder`, which turns boot reports into characters],
)

The board finds a controller by its PCI class, `PCI_CLASS` (`0x000c_0330`: serial bus, USB, xHCI), maps its BAR, and enables memory decoding and DMA through @crate-pci. From there the public interface is three calls: `Xhci::new` starts the controller, `find_keyboard` looks for a keyboard and returns its port, and `poll` returns the next character typed.

== TRBs and rings

Driver and controller talk through *Transfer Request Blocks*: 16-byte records in memory, laid out in rings.

#bytefield(caption: "A TRB, 16 bytes, little-endian",
  ("param", "u64", 8), ("status", "u32", 4), ("control", "u32", 4),
)

#bytefield(caption: "The control word, as this driver reads it",
  ("slot", "bits 31–24", 4), ("endpoint", "bits 20–16", 3), ("type", "bits 15–10", 3), ("flags", "bits 9–1", 4), ("C", "bit 0", 1),
)

`Trb` has accessors for those fields (`kind`, `slot`, `endpoint`, and `code`, the top byte of `status`) and a constructor per record the driver sends:

#dtable(columns: (auto, 1fr, 1.4fr), ("Constructor", "Type", "What it asks for"),
  [`setup`], [`SETUP` (2)], [the first stage of a control transfer, with the 8-byte request in `param`],
  [`data`], [`DATA` (3)], [its data stage, in or out of a buffer],
  [`status_stage`], [`STATUS` (4)], [its last stage, in the opposite direction, interrupting on completion],
  [`normal`], [`NORMAL` (1)], [one interrupt-endpoint transfer: the next report, into a buffer],
  [`enable_slot`], [`ENABLE_SLOT` (9)], [a device slot],
  [`with_context`], [`ADDRESS_DEVICE` (11), `CONFIGURE_ENDPOINT` (12), `EVALUATE_CONTEXT` (13)], [a command that takes an input context, for a slot],
)

The other constants are what comes back or links the ring: `LINK` (6), the events `TRANSFER_EVENT` (32) and `COMMAND_COMPLETION` (33), and the completion codes `SUCCESS` (1) and `SHORT_PACKET` (13).

There is no head or tail register. Ownership of a TRB is decided by its *cycle bit*: the producer writes its current cycle, the consumer takes a TRB only if the bit matches what it expects, and each time the ring wraps the expected value flips. A slot left over from the previous pass carries the old cycle, so it reads as not yet written.

=== The driver's rings

`Ring` is what the driver produces on: the command ring and the transfer rings. Its last slot is always a *link* TRB pointing back to the start.

#srcfile("drivers/xhci/src/trb.rs", lines: ((125, 149),), notes: ((132, [The cycle bit is in `control`, written last and after a fence, so the controller never sees a TRB with a valid cycle and a stale body.]), (142, [The link is written as soon as the slot before it is used.]), (143, [`TOGGLE_CYCLE` tells the controller to flip its expected cycle when it follows this link.]), (146, [And the driver flips the one it writes from now on.]),))[```rust
fn write(&mut self, at: usize, trb: Trb) {
    // SAFETY: `at` is within the ring. The cycle bit, in `control`, is written last.
    unsafe {
        let slot = self.trbs.add(at);
        write_volatile(&raw mut (*slot).param, trb.param);
        write_volatile(&raw mut (*slot).status, trb.status);
        fence(Ordering::Release);
        write_volatile(&raw mut (*slot).control, trb.control & !CYCLE | self.cycle);
    }
}

/// Add a TRB, and return its address. The last slot is always the link.
pub fn push(&mut self, trb: Trb) -> u64 {
    // SAFETY: within the ring.
    let address = unsafe { self.trbs.add(self.index) } as u64;
    self.write(self.index, trb);
    self.index += 1;
    if self.index == self.len - 1 {
        let link = Trb { param: self.trbs as u64, status: 0, control: kind(LINK) | TOGGLE_CYCLE };
        self.write(self.index, link);
        self.index = 0;
        self.cycle ^= CYCLE;
    }
    address
}
```]

`dequeue_pointer` is the ring's address with the cycle bit in bit 0, which is the form the CRCR register and an endpoint context want.

=== The event ring

`EventRing` is the other direction: the controller produces and the driver consumes. It has no link TRB, because the controller wraps by the size in a *segment table*.

#srcfile("drivers/xhci/src/trb.rs", lines: ((174, 195),), notes: ((179, [A different cycle means the controller hasn't written this one yet.]), (182, [The cycle bit is read first, and the fence orders the rest of the TRB after it.]), (192, [The ring wrapped, so what counts as new has the opposite bit.]),))[```rust
pub fn pop(&mut self) -> Option<Trb> {
    // SAFETY: within the ring. The cycle bit is read first, and says the rest is there.
    let trb = unsafe {
        let slot = self.trbs.add(self.index);
        let control = read_volatile(&raw const (*slot).control);
        if control & CYCLE != self.cycle {
            return None;
        }
        fence(Ordering::Acquire);
        Trb {
            param: read_volatile(&raw const (*slot).param),
            status: read_volatile(&raw const (*slot).status),
            control,
        }
    };
    self.index += 1;
    if self.index == self.len {
        self.index = 0;
        self.cycle ^= CYCLE;
    }
    Some(trb)
}
```]

After each event, `next_event` writes the new position to the `ERDP` register, so that the controller knows the slot is free again. `ERDP_BUSY`, bit 3, is cleared by writing 1.

== Descriptors

A USB device describes itself with a *configuration descriptor*: a run of records, each starting with its length and type. `find_boot_keyboard` walks one looking for an interface of class HID (3), subclass boot (1), protocol keyboard (1), and takes the first interrupt IN endpoint that follows it.

#srcfile("drivers/xhci/src/descriptors.rs", lines: ((28, 56),), notes: ((29, [A length under 2 would never advance.]), (32, [A record that claims to run past the end stops the walk.]), (35, [Alternate setting 0 only. Each interface record replaces the last, so an endpoint is credited to the interface it follows.]), (43, [The low 11 bits of `wMaxPacketSize` are the size. The bits above them are for high-bandwidth endpoints.]),))[```rust
while let (Some(&len), Some(&kind)) = (config.get(at), config.get(at + 1)) {
    if len < 2 {
        return None;
    }
    let d = config.get(at..at + len as usize)?;
    match kind {
        INTERFACE if d.len() >= 9 => {
            let keyboard = d[3] == 0 && d[5] == CLASS_HID && d[6] == SUBCLASS_BOOT && d[7] == PROTOCOL_KEYBOARD;
            interface = keyboard.then_some(d[2]);
        }
        ENDPOINT if d.len() >= 7 => {
            if let Some(interface) = interface
                && d[2] & ENDPOINT_IN != 0
                && d[3] & 3 == ATTRIBUTES_INTERRUPT
            {
                let max_packet = u16::from_le_bytes([d[4], d[5]]) & 0x7ff;
                return Some(BootKeyboard {
                    configuration,
                    interface,
                    endpoint: d[2] & 0xf,
                    max_packet,
                    interval: d[6],
                });
            }
        }
        _ => {}
    }
    at += len as usize;
}
```]

The result is a `BootKeyboard`: the `configuration` to select, the `interface`, the `endpoint` number without its direction bit, `max_packet` and `interval`.

The two helpers translate between USB's and xHCI's units. `default_max_packet` is the largest control packet at a port speed until the device says otherwise: 8 bytes at full and low speed (1 and 2), 64 at high speed (3), 512 above. `endpoint_interval` converts `bInterval` to an endpoint context's exponent, polling every $2^n times 125$ µs:

#srcfile("drivers/xhci/src/descriptors.rs", lines: ((62, 70),), notes: ((66, [Full and low speed give a count of milliseconds. 10 ms rounds down to 8 ms, an exponent of 6.]), (68, [High speed and up already give the exponent, plus one.]),))[```rust
pub fn endpoint_interval(speed: u8, b_interval: u8) -> u8 {
    const FULL: u8 = 1;
    const LOW: u8 = 2;
    if speed == FULL || speed == LOW {
        (b_interval.max(1).ilog2() as u8 + 3).clamp(3, 10)
    } else {
        b_interval.clamp(1, 16) - 1
    }
}
```]

== The HID boot report

We put a keyboard in *boot protocol*, a fixed format that needs no parsing of the device's report descriptor. Every report is 8 bytes:

#bytefield(caption: "A HID boot keyboard report",
  ("modifiers", "byte 0", 2), ("reserved", "byte 1", 2), ("keys", "bytes 2–7: up to six usages", 6),
)

A report says which keys are down, not which went down. So `Decoder` remembers the last report, and a key is typed when it appears in a report and wasn't in the one before:

#srcfile("drivers/xhci/src/hid.rs", lines: ((26, 37),), notes: ((28, [Usage 1 means too many keys are down to list. That says nothing about which, so the previous state is kept.]), (31, [Only the keys new in this report.]), (36, [A key that was released and pressed again now counts as new.]),))[```rust
pub fn feed(&mut self, report: &[u8; 8], mut typed: impl FnMut(u8)) {
    let keys: [u8; 6] = [report[2], report[3], report[4], report[5], report[6], report[7]];
    if keys.contains(&ROLLOVER) {
        return; // says nothing about which keys are down: keep what we knew
    }
    for &key in keys.iter().filter(|&&k| k != 0 && !self.held.contains(&k)) {
        if let Some(c) = character(report[0], key) {
            typed(c);
        }
    }
    self.held = keys;
}
```]

`character` maps usages 4 (`a`) to 56 (`/`) through a `PLAIN` or `SHIFTED` table on a US layout. Bits 1 and 5 of the modifier byte are the two shift keys, and bits 0 and 4 the two Ctrl keys. As with @crate-i8042, Ctrl turns a letter into its control code.

== Bringing the controller up

`Xhci::new` runs at boot, on the only CPU, with the controller's registers mapped. Its capability registers say where the others are: the *operational* registers (at an offset in the first byte), the *runtime* registers, and the *doorbells*.

=== Memory: the `Arena`

Everything the controller reads or writes lives in an `Arena`, a bump allocator over memory the caller provides, which hands out zeroed blocks and never takes them back.

#srcfile("drivers/xhci/src/lib.rs", lines: ((97, 109),), notes: ((99, [A ring must not cross a 64 KiB boundary. The arena applies that to every block, and moves one that would up to the next boundary.]), (102, [A block over 64 KiB can't satisfy that at all.]),))[```rust
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
```]

It can't be the kernel's heap. The `pc` board demand-pages its heap, and a device's DMA doesn't take page faults: the controller would write to a page that isn't there. So the board gives the arena 1 MiB of `.bss`, which is mapped from the start and identity-mapped, so that its addresses are also the physical ones the controller needs.

=== The firmware handoff

On a PC the firmware may be using the controller, to offer a USB keyboard at its setup screen, with system management interrupts behind the operating system's back. xHCI has an extended capability for asking for it:

#srcfile("drivers/xhci/src/lib.rs", lines: ((491, 508),), notes: ((492, [`offset` is the first extended capability, in dwords, from `HCCPARAMS1`.]), (496, [Setting the OS-owned bit asks. The firmware answers by clearing its own.]), (497, [The result is ignored: a firmware that doesn't let go within a second is overridden.]), (499, [Turns the SMIs off, and clears any pending events, which clear on a write of 1.]),))[```rust
fn take_from_firmware(base: usize, mut offset: usize, delay_ms: fn(u64)) {
    while offset != 0 {
        let at = base + offset * 4;
        let capability = read32(at);
        if capability & 0xff == LEGACY_CAPABILITY {
            write32(at, capability | LEGACY_OS_OWNED);
            let _ = wait(delay_ms, || read32(at) & LEGACY_BIOS_OWNED == 0);
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
```]

QEMU's controller has no such capability: its list holds two Supported Protocol entries, so in our runs the loop walks past them and returns without asking anyone.

=== Reset and scratchpads

The driver stops the controller, waits for it to halt, resets it, and waits for the reset to finish and `CNR` to clear. Each wait is up to a second, through the `delay_ms` function the board passes in, and each failure is an error. It then enables up to 16 device slots and allocates the *device context base address array* (DCBAA), with one entry per slot plus entry 0.

#srcfile("drivers/xhci/src/lib.rs", lines: ((180, 194),), notes: ((183, [The count is split across two fields of `HCSPARAMS2`: five high bits at 25–21, five low bits at 31–27.]), (187, [Each scratchpad is a whole page, which belongs to the controller from now on.]), (192, [Entry 0 of the DCBAA is reserved for this array when there are scratchpads.]),))[```rust
let slots = (params1 & 0xff).min(MAX_SLOTS);
write32(operational + CONFIG, slots);
let dcbaa = arena.alloc((slots as usize + 1) * 8, 64)?;
let scratchpads = ((params2 >> 21 & 0x1f) << 5 | params2 >> 27) as usize;
if scratchpads > 0 {
    let array = arena.alloc(scratchpads * 8, 64)?;
    for i in 0..scratchpads {
        let page = arena.alloc(PAGE, PAGE)?;
        // SAFETY: the array has `scratchpads` entries, in the arena.
        unsafe { write_volatile((array as *mut u64).add(i), page as u64) };
    }
    // SAFETY: entry 0 of the DCBAA, in the arena, is the scratchpad array.
    unsafe { write_volatile(dcbaa as *mut u64, array as u64) };
}
write64(operational + DCBAAP, dcbaa as u64);
```]

*Scratchpads* are memory the controller asks for its own use. The driver never reads them, but it has to provide them. QEMU's controller asks for none (its `HCSPARAMS2` reads `0xf`), so this path hasn't run.

The rest of `new` sets up the two main rings: the command ring through `CRCR`, and the event ring through a one-entry segment table written to `ERSTSZ`, `ERDP` and `ERSTBA`, in that order. It powers any port that is off, sets the run bit, waits for the controller to leave the halted state, and gives devices 200 ms to connect.

== Bringing a device up

`find_keyboard` goes through the ports, and for each connected one calls `reset_port` and then `try_port`, stopping at the first keyboard. A USB 3 port trains its link by itself and is already enabled. A USB 2 port needs a reset, and 20 ms of recovery after it.

Commands go on the command ring, then the doorbell at index 0 tells the controller to look. `command` waits up to a second for the completion event whose `param` is that command's address. `control` does a control transfer on endpoint 0 with the same kind of wait.

The heart of the driver is `try_port`, which walks the sequence the xHCI specification lays out:

+ *Enable Slot*: the controller assigns a slot, and we write the address of a new output device context to its DCBAA entry.
+ *Address Device*: an input context describes the slot (speed, port) and endpoint 0, and the command gives the device its USB address.
+ *Read the packet size*: the first 8 bytes of the device descriptor say how big endpoint 0's packets are, and if that differs from the default, an *Evaluate Context* command updates it.
+ *Read the configuration*: 9 bytes for the total length, then the whole descriptor, up to 512 bytes, for `find_boot_keyboard`.
+ *Configure*: `SET_CONFIGURATION`, then `SET_PROTOCOL` to the boot protocol, then `SET_IDLE`, so that the keyboard reports only on a change. A keyboard may refuse the last one, and the driver carries on.
+ *Configure Endpoint*: a command adds the keyboard's interrupt endpoint to the slot, and the first transfer is queued.

#srcfile("drivers/xhci/src/lib.rs", lines: ((383, 399),), size: 6.4pt, notes: ((383, [The input control context's add flags: the slot context, and endpoint 0.]), (384, [Speed, and "one endpoint" in the slot context's entry count.]), (387, [After this the device answers at its own address.]), (394, [USB 3 gives the size as a power of two, and the rest as a byte count.]),))[```rust
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
```]

Once the keyboard is configured, `start_keyboard` adds its endpoint. The endpoint's index in the device context is twice its number plus one for IN, and the slot context, copied from what the controller holds, has its entry count raised to match:

#srcfile("drivers/xhci/src/lib.rs", lines: ((456, 473),), size: 6.4pt, notes: ((456, [Adds the slot context and this one endpoint.]), (459, [The entry count is the highest endpoint index in use.]), (467, [Three retries, type Interrupt IN, and the packet size.]), (472, [The first read. It completes when the keyboard next has something to say.]),))[```rust
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
```]

== Reading the keyboard

`poll` drains the event ring, handing each event to `on_keyboard_event`, then returns one character from a queue of six.

#srcfile("drivers/xhci/src/lib.rs", lines: ((273, 290),), notes: ((276, [Any other code means the endpoint has stopped, and nothing is requeued.]), (280, [The fixed 8-byte report, which the controller has finished writing.]), (283, [The queue holds six characters. A seventh is dropped.]), (288, [The next read goes on the ring, and the doorbell tells the controller.]),))[```rust
if event.kind() != trb::TRANSFER_EVENT || event.slot() != k.slot || event.endpoint() != k.endpoint {
    return;
}
if !matches!(event.code(), trb::SUCCESS | trb::SHORT_PACKET) {
    return; // the endpoint has stopped: the keyboard is gone, or broken
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
```]

Each report requeues one `normal` TRB, so one transfer is outstanding at any moment.

== Exercised by

The host tests cover what doesn't need a controller: TRB encoding, a ring's link and cycle flip, the event ring's ownership, descriptor parsing (a keyboard, a mouse, a second interface, an OUT endpoint, truncation), the interval conversion, the report decoder, and the arena's alignment and 64 KiB rule.

#output(title: "cargo test -p green-tea-xhci")[
```
test result: ok. 27 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

What needs a controller runs in QEMU. Every run on the `pc` board attaches `qemu-xhci` and a `usb-kbd`, so every one brings the device up and prints its port. To show a key arriving through this driver alone, we boot the `shell` kernel on a `q35` with the PS/2 controller removed (`i8042=off`), and type `echo keys` from QEMU's monitor:

#output(title: "docs/record.d/xhci.sh: QEMU's sendkey, with i8042=off")[
```
usb: keyboard on port 5
green-tea-shell on x86_64 (pc)
type `help` for the commands
> echo keys
keys
```
]

Port 5 is the first USB 2 port: QEMU's controller numbers its USB 3 ports 1 to 4 and its USB 2 ports 5 to 8, and `usb-kbd` is a USB 2 device.

== Limits

- Tested only against QEMU's `qemu-xhci` with `usb-kbd`, never on physical hardware. The firmware handoff and the scratchpads have not run, because QEMU's controller needs neither.
- Polled: no interrupts. A key waits in the event ring until someone calls `poll`.
- Root ports only: no hubs. A keyboard behind a hub isn't found.
- No hot-plugging: the keyboard has to be attached at boot.
- A keyboard only: no USB storage, and nothing else is driven. A device that isn't a keyboard is left addressed in its slot, with its memory still allocated.
- Only the first keyboard, on the first controller that has one, and only in the device's first configuration.
- The arena is never reclaimed, and it is 1 MiB on the `pc` board, shared by every controller. A controller that asks for 256 scratchpad pages runs it out, and `new` fails.
- A stalled or failed keyboard endpoint stops silently: `on_keyboard_event` doesn't reset it, and `poll` returns nothing from then on. The same goes for endpoint 0 after a refused `SET_IDLE`, which the driver doesn't use again.
- The transfer length in an event is ignored (line 280), so a report shorter than 8 bytes is decoded with the tail of the previous one.
- A device whose configuration descriptor reports a total length of 0 gets a setup TRB that says "no data", followed by a data TRB (lines 405 and 406).
- `poll` queues at most six characters between calls, and drops the rest.
- Memory ordering uses `fence(SeqCst)`, which is enough on `x86_64`, the only board that uses the crate. It doesn't cover device I/O on `riscv64`, which is what `dma_fence` in @crate-io is for.

Six steps to read eight bytes. @crate-bare is what puts these drivers on a machine.
