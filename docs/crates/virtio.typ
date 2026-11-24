#import "../lib.typ": *

= green-tea-virtio <crate-virtio>

`green-tea-virtio` implements the virtio device model: the status handshake, feature negotiation, split virtqueues, and the two transports, MMIO on `riscv64` and `aarch64` `virt` and PCI on `x86_64` `q35`. A device driver such as @crate-virtio-blk is written once against the `Transport` trait and runs on all three. Only modern virtio (1.0 and later) is implemented, so everything on the wire is little-endian and the `VERSION_1` feature is required.

#spec(
  ("Source", [#repo("drivers/virtio/src/"), 4 files, 991 lines]),
  ("Depends on", [`green-tea-io`, `green-tea-pci`, and `alloc`: a queue is heap-allocated]),
  ("Used by", [`green-tea-virtio-blk`, `green-tea-bare` (each QEMU board's `for_each_virtio`)]),
  ("Tests", [15 host tests, on fake devices in memory; the `disk` kernel, on all three architectures]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [crate root], [`Transport`, `negotiate`, `failed`, `driver_ok`, `AnyTransport`, `Device`, `device_name`, the `ID_*`, `STATUS_*` and `F_VERSION_1` constants],
  [`mmio`], [`MmioTransport`, and `probe` for one slot],
  [`pci`], [`PciTransport`, and `probe` for one PCI function],
  [`queue`], [`Queue`, `Buffer`, `SIZE`],
)

A virtio device has four parts: a status byte, which the driver steps through a fixed handshake; 64 feature bits, which the device offers and the driver accepts a subset of; a device-specific configuration area; and virtqueues, rings in guest memory through which the driver hands the device buffers and gets them back.

The device ids are the same on every transport: `ID_NET` (1), `ID_BLOCK` (2), `ID_CONSOLE` (3), `ID_RNG` (4), `ID_GPU` (16) and `ID_INPUT` (18). `device_name` turns one into a word for log messages (`network`, `block`, `console`, `entropy`, `GPU`, `input`), and any other id into `unknown`. The workspace has a driver for one of the six, `ID_BLOCK`.

== The `Transport` trait

`Transport` names the registers by meaning rather than by address:

#srcfile("drivers/virtio/src/lib.rs", lines: ((61, 61), (63, 63), (65, 65), (67, 67), (69, 69), (72, 72), (74, 74), (77, 77), (79, 79), (83, 83), (85, 85), (89, 99),), notes: ((63, [`&mut self` on the feature and queue methods: on both transports they write a selector register first.]), (74, [A separate question from the size, so that a queue in use isn't mistaken for a missing one.]), (89, [The one provided method. The generation counter detects a configuration change between the two 32-bit reads, and the loop retries.]),))[```rust
pub trait Transport {
// …
    fn device_features(&mut self) -> u64;
// …
    fn set_driver_features(&mut self, features: u64);
// …
    fn status(&self) -> u8;
// …
    fn set_status(&mut self, status: u8);
// …
    fn max_queue_size(&mut self, queue: u16) -> u16;
// …
    fn queue_in_use(&mut self, queue: u16) -> bool;
// …
    fn setup_queue(&mut self, queue: u16, size: u16, descriptors: u64, driver: u64, device: u64);
// …
    fn notify(&self, queue: u16);
// …
    fn config_generation(&self) -> u32;
// …
    fn read_config_u32(&self, offset: usize) -> u32;
// …
    fn read_config_u64(&self, offset: usize) -> u64 {
        loop {
            let before = self.config_generation();
            let low = self.read_config_u32(offset);
            let high = self.read_config_u32(offset + 4);
            if self.config_generation() == before {
                return u64::from(high) << 32 | u64::from(low);
            }
        }
    }
}
```]

== Negotiation

The driver moves the status byte through a fixed sequence, setting one more bit each time:

#dtable(columns: (auto, auto, 1fr), ("Constant", "Value", "The driver is saying"),
  [`STATUS_ACKNOWLEDGE`], [1], [it has seen the device],
  [`STATUS_DRIVER`], [2], [it knows how to drive it],
  [`STATUS_FEATURES_OK`], [8], [the features it wrote are final],
  [`STATUS_DRIVER_OK`], [4], [the queues are set up: go],
  [`STATUS_FAILED`], [128], [it has given up on the device],
)

`negotiate` does everything up to the queues, `driver_ok` the last step, and `failed` gives up on a device by adding `STATUS_FAILED` to the bits already set, because the spec forbids a driver to clear one:

#srcfile("drivers/virtio/src/lib.rs", lines: ((107, 134), (137, 140),), notes: ((109, [Writing 0 resets the device, and the reset is complete when the status reads 0 again.]), (118, [The agreed set: what the device offers, intersected with what the driver supports plus `F_VERSION_1`, which is bit 32 and always accepted.]), (122, [The device clears `FEATURES_OK` if it can't work with that subset, so the driver has to read it back.]), (139, [Read-modify-write, keeping the bits already set.]),))[```rust
pub fn negotiate(transport: &mut impl Transport, supported: u64) -> Result<u64, &'static str> {
    transport.set_status(0);
    while transport.status() != 0 {} // a reset may take a moment to finish
    transport.set_status(STATUS_ACKNOWLEDGE);
    transport.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    let offered = transport.device_features();
    if offered & F_VERSION_1 == 0 {
        failed(transport);
        return Err("legacy-only virtio device (no VERSION_1 feature)");
    }
    let features = offered & (supported | F_VERSION_1);
    transport.set_driver_features(features);
    transport.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    // The device clears FEATURES_OK if it can't work with that subset.
    if transport.status() & STATUS_FEATURES_OK == 0 {
        failed(transport);
        return Err("the virtio device rejected our features");
    }
    Ok(features)
}

/// Tell the device we've given up on it. The spec forbids the driver to clear a status
/// bit, so this adds to the ones already set.
pub fn failed(transport: &mut impl Transport) {
    let status = transport.status();
    transport.set_status(status | STATUS_FAILED);
}
// …
pub fn driver_ok(transport: &mut impl Transport) {
    let status = transport.status();
    transport.set_status(status | STATUS_DRIVER_OK);
}
```]

The caller's order is fixed: `negotiate`, then `Queue::new` for each queue, then `driver_ok`.

== The MMIO transport

The `mmio` module drives a block of `0x200` bytes of 32-bit registers per device. QEMU's `virt` has a row of slots: 8, 4 KiB apart from `0x1000_1000`, on `riscv64`, and 32, `0x200` apart from `0x0a00_0000`, on `aarch64`. An empty slot reads as device id 0.

#dtable(columns: (auto, 1fr, auto, 1fr), ("Offset", "Register", "Offset", "Register"),
  [`0x000`], [magic, `virt`], [`0x044`], [queue ready],
  [`0x004`], [version], [`0x050`], [queue notify],
  [`0x008`], [device id], [`0x070`], [status],
  [`0x010`, `0x014`], [device features, selector], [`0x080`, `0x084`], [descriptor table, low and high],
  [`0x020`, `0x024`], [driver features, selector], [`0x090`, `0x094`], [driver area, low and high],
  [`0x030`], [queue selector], [`0x0a0`, `0x0a4`], [device area, low and high],
  [`0x034`, `0x038`], [queue size, maximum and current], [`0x0fc`, `0x100`], [config generation, config],
)

`probe` returns `Ok(None)` for a wrong magic value or device id 0, and the id and transport for version 2. Version 1, a legacy device, is an error whose message names the fix: QEMU makes its MMIO slots legacy unless started with `-global virtio-mmio.force-legacy=false`, which `cargo xtask kernel` passes.


The 64-bit feature sets are reached through a 32-bit window with a selector, and a queue's registers through `QUEUE_SEL`. `setup_queue` writes the size and the three addresses, and writes `QUEUE_READY` last, because the device may use the queue from that write on.

`queue_in_use` selects the queue and reads `QUEUE_READY`. `max_queue_size` selects it and returns `QUEUE_NUM_MAX`, which the device reports as 0 for a queue that doesn't exist.

== The PCI transport

On PCI, a virtio device is a function with vendor `0x1af4`. Its id comes from the PCI device id: `0x1040` plus the virtio id for a modern-only device, or the subsystem id for a transitional one (`0x1000` to `0x103f`). Its registers are in its BARs, in up to four structures, and vendor-specific capabilities (id `0x09`) say where each one is.

#bytefield(caption: "A virtio PCI capability, in configuration space; a notify capability adds a u32 multiplier at +16",
  ("id", "u8", 1), ("next", "u8", 1), ("len", "u8", 1), ("cfg_type", "u8", 2), ("bar", "u8", 1), ("id", "u8", 1), ("pad", "2", 1), ("offset", "u32", 3), ("length", "u32", 3),
)

The driver uses three of the structures: type 1 (`CFG_COMMON`), type 2 (`CFG_NOTIFY`) and type 4 (`CFG_DEVICE`). It ignores the ISR status, since it polls.

#srcfile("drivers/virtio/src/pci.rs", lines: ((74, 93),), size: 6.8pt, notes: ((79, [`bar < 6` guards the index, and `ecam.bar` returns `None` for an I/O BAR, so such a capability is skipped.]), (92, [A device without common and notify structures is refused here, and stays off.]), (93, [The device needs memory decoding and DMA from here on.]),))[```rust
ecam.for_each_capability(f, |cap, cap_id| {
    if cap_id != CAPABILITY_VENDOR {
        return;
    }
    let bar = usize::from(ecam.read_u8(f, cap + CAP_BAR));
    let Some(base) = (bar < 6).then(|| ecam.bar(f, bar)).flatten() else { return };
    let address = (base + u64::from(ecam.read_u32(f, cap + CAP_OFFSET))) as usize;
    // The first of each type is the one to use.
    match ecam.read_u8(f, cap + CAP_CFG_TYPE) {
        CFG_COMMON => _ = common.get_or_insert(address),
        CFG_NOTIFY if notify.is_none() => {
            notify = Some(address);
            notify_multiplier = ecam.read_u32(f, cap + CAP_NOTIFY_MULTIPLIER);
        }
        CFG_DEVICE => _ = device.get_or_insert(address),
        _ => {}
    }
});
let (common, notify) = (common?, notify?);
ecam.enable(f);
```]

The common configuration structure has its fields at fixed offsets, of mixed widths, so `PciTransport` has generic `read<T>` and `write<T>` over it. A queue's setup looks like the MMIO one, with one addition:

#srcfile("drivers/virtio/src/pci.rs", lines: ((168, 170),), size: 6.1pt, notes: ((169, [Each queue has its own notification address: the notify structure, plus `queue_notify_off` times the capability multiplier. It is saved in an array of `MAX_QUEUES` (4), so that `notify` needn't reselect the queue. An address of 0 there means the queue was never set up.]),))[```rust
let offset: u16 = self.read(QUEUE_NOTIFY_OFF);
self.queue_notify[usize::from(queue)] = self.notify + usize::from(offset) * self.notify_multiplier as usize;
self.write(QUEUE_ENABLE, 1u16);
```]

`probe` requires the common and notify structures, and allows a device with no device-specific area, recording address 0. `config_generation` is 8 bits wide here, and `read_config_u32` asserts that the device has a device-specific area. `setup_queue` asserts that the index is below `MAX_QUEUES` before it writes a register, `queue_in_use` answers `false` past them, and `notify` panics on a queue that was never set up.

== `AnyTransport` and `Device`

A board finds devices at run time, and the two transports are different types, so `AnyTransport` is an enum of `Mmio` and `Pci` that implements `Transport` by delegating every method through a small macro, `delegate!`.


`Device` pairs a virtio id with an `AnyTransport`. It is what each board's `for_each_virtio` passes to its callback, and `find_virtio` in `green-tea-bare` returns the transport of the first device with a given id.

== Split virtqueues

The `queue` module implements the *split virtqueue*: three areas in guest memory, each written by one side only. The driver writes the descriptor table and the available ring, and the device writes the used ring.

#bytefield(caption: "A descriptor, 16 bytes, little-endian",
  ("address", "u64", 8), ("length", "u32", 4), ("flags", "u16", 2), ("next", "u16", 2),
)

Two flags are used: `DESC_F_NEXT` (1), the chain continues at `next`, and `DESC_F_WRITE` (2), the device writes this buffer rather than reads it. `SIZE` is 16 descriptors per queue. The three areas share one `Rings` allocation, 432 bytes, aligned to 16:

#bytefield(caption: "Rings for SIZE = 16: offsets 0, 256 and 296",
  ("descriptors", "16 × 16 = 256", 256), ("available", "38", 60), ("pad", "2", 14), ("used", "136", 136),
)

The available ring is `flags`, `index`, sixteen `u16` slots and `used_event`. The used ring is `flags`, `index`, sixteen `{ id: u32, length: u32 }` elements and `avail_event`.

The spec asks for alignments of 16, 2 and 4 for the three parts. `repr(C, align(16))` gives the first, and the field types give the other two: the 2 bytes of padding after the available ring put the used ring on a 4-byte boundary. `used_event` and `avail_event` belong to the `EVENT_IDX` feature, which the driver doesn't negotiate.

=== Creating a queue

`Queue::new` refuses a queue already in use, checks the device's maximum, allocates the rings zeroed, links every descriptor into a free list through its `next` field, and hands the three addresses to the transport:

#srcfile("drivers/virtio/src/queue.rs", lines: ((133, 147),), notes: ((136, [`next` doubles as the free-list link while a descriptor is unused. The list is circular.]), (138, [`AVAIL_F_NO_INTERRUPT`: the driver polls the used ring, so it asks for no interrupts.]),))[```rust
let rings = Box::into_raw(unsafe { Box::<Rings>::new_zeroed().assume_init() });
unsafe {
    for i in 0..N {
        (*rings).descriptors[i].next = (i as u16 + 1) % SIZE;
    }
    (*rings).available.flags = AVAIL_F_NO_INTERRUPT;
    transport.setup_queue(
        index,
        SIZE,
        addr_of_mut!((*rings).descriptors) as u64,
        addr_of_mut!((*rings).available) as u64,
        addr_of_mut!((*rings).used) as u64,
    );
}
Ok(Queue { index, rings, free_head: 0, free_count: SIZE, last_used: 0 })
```]

The rings are never freed, because the device may still hold their addresses.

=== Submitting a chain

A request is a slice of `Buffer`s, each with an `address`, a `length` and `device_writes`. `Buffer::reads` makes one the device reads from a `&T`, and `Buffer::writes` one the device writes from a `&mut T`. Both panic on 4 GiB or more, which a descriptor's `u32` length can't hold. `submit` takes that many descriptors off the free list:

#srcfile("drivers/virtio/src/queue.rs", lines: ((160, 191),), notes: ((163, [Each descriptor's `next` already names the next free one, so a chain is a prefix of the free list and the loop never writes `next`.]), (185, [Indexes are free-running `u16`s, taken modulo the queue size for a slot.]), (186, [The descriptors and the ring slot have to be visible before the index that publishes them.]), (189, [And the index before the notification.]),))[```rust
for (i, buffer) in buffers.iter().enumerate() {
    unsafe {
        let descriptor = addr_of_mut!((*rings).descriptors[usize::from(current)]);
        let next = read_volatile(addr_of_mut!((*descriptor).next));
        let mut flags = if buffer.device_writes { DESC_F_WRITE } else { 0 };
        if i + 1 < buffers.len() {
            flags |= DESC_F_NEXT;
        }
        write_volatile(addr_of_mut!((*descriptor).address), buffer.address);
        write_volatile(addr_of_mut!((*descriptor).length), buffer.length);
        write_volatile(addr_of_mut!((*descriptor).flags), flags);
        // The last descriptor's `next` keeps pointing into the free list; without
        // DESC_F_NEXT the device ignores it.
        if i + 1 < buffers.len() {
            current = next;
        } else {
            self.free_head = next;
        }
    }
}
self.free_count -= buffers.len() as u16;

unsafe {
    let available = addr_of_mut!((*rings).available);
    let index = read_volatile(addr_of_mut!((*available).index));
    write_volatile(addr_of_mut!((*available).ring[usize::from(index % SIZE)]), head);
    dma_fence(); // the descriptors and ring slot before the index that publishes them
    write_volatile(addr_of_mut!((*available).index), index.wrapping_add(1));
}
dma_fence(); // the index before the notification
transport.notify(self.index);
Ok(head)
```]

=== Reclaiming a chain

`pop_used` compares the device's index with the last one read. If it moved, it reads the element, and walks the chain to put it back on the free list:

#srcfile("drivers/virtio/src/queue.rs", lines: ((199, 224),), notes: ((203, [The fence between the index and what it covers: the element, and the data the device wrote.]), (215, [The chain's last descriptor is the only one whose link changes. It is spliced in front of the free list.]),))[```rust
    let used = addr_of_mut!((*rings).used);
    if read_volatile(addr_of_mut!((*used).index)) == self.last_used {
        return None;
    }
    dma_fence(); // the index before the element it covers, and the data written
    let element = addr_of_mut!((*used).ring[usize::from(self.last_used % SIZE)]);
    let head = read_volatile(addr_of_mut!((*element).id)) as u16;
    let length = read_volatile(addr_of_mut!((*element).length));
    self.last_used = self.last_used.wrapping_add(1);

    // Put the chain back at the front of the free list.
    let mut last = head;
    let mut count = 1;
    loop {
        let descriptor = addr_of_mut!((*rings).descriptors[usize::from(last)]);
        if read_volatile(addr_of_mut!((*descriptor).flags)) & DESC_F_NEXT == 0 {
            write_volatile(addr_of_mut!((*descriptor).next), self.free_head);
            break;
        }
        last = read_volatile(addr_of_mut!((*descriptor).next));
        count += 1;
    }
    self.free_head = head;
    self.free_count += count;
    Some((head, length))
}
```]

Since a finished chain goes back to the front of the free list in its original order, a driver that does one request at a time uses the same descriptors for every request. In the `disk` kernel the head is always descriptor 0, and descriptors 3 to 15 never carry a request.

== Exercised by

The host tests run the negotiation, both transports and the buffers against fake devices in ordinary memory: a refused feature set keeps the status bits, a queue in use isn't reported as missing, an incomplete PCI device stays off, and a notification for a queue that was never set up panics.

#output(title: "cargo test -p green-tea-virtio")[
```
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `disk` kernel is the only one that uses this crate on a machine, and it reaches the device over a different transport per machine. `cargo xtask kernel` attaches a 32 KiB image, passing `-global virtio-mmio.force-legacy=false` on `virt` and `disable-legacy=on` on `q35`, so both transports are modern. The kernel makes 20 requests: the disk's id, 16 reads of 4 sectors, one write, one flush and one read back. That is more than `SIZE`, so both rings wrap: the seventeenth request goes back to slot 0.

The `aarch64` run prints the same three lines as `riscv64`.

#output(title: "cargo xtask kernel --kernel disk --target riscv64 --headless")[
```
virtio: block device (id 2)
virtio-blk over MMIO: 64 sectors (32 KiB), id "green-tea-disk", write cache
PASS: read 64 sectors over virtio-MMIO, and wrote one back
```
]

#output(title: "cargo xtask kernel --kernel disk --target x86_64 --headless")[
```
virtio: block device (id 2)
virtio-blk over PCI: 64 sectors (32 KiB), id "green-tea-disk", write cache
PASS: read 64 sectors over virtio-PCI, and wrote one back
```
]

== Limits

- Split virtqueues only, with no packed rings, indirect descriptors or `EVENT_IDX`. The driver polls, and no transport sets up interrupts.
- A device whose largest queue is under 16 is refused, although a smaller queue would serve a three-descriptor request.
- `pci::probe` enables memory decoding and bus mastering on every complete virtio function a scan finds, including those nobody goes on to drive.
- The PCI transport handles four queues, `MAX_QUEUES`. Past them, `max_queue_size` says 0 and `setup_queue` panics.
- The waits have no timeout: the reset loop in `negotiate` spins until the device answers.

One device model, two transports, three machines. @crate-virtio-blk puts a disk on it.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: c1287b826d3d
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
