#import "../lib.typ": *

= green-tea-virtio-blk <crate-virtio-blk>

`green-tea-virtio-blk` drives a virtio disk over any `Transport`: it reads and writes whole 512-byte sectors, flushes the write cache and reads the device's serial number. It makes one request at a time and polls for the answer.

#spec(
  ("Source", [#repo("drivers/virtio-blk/src/lib.rs"), 191 lines]),
  ("Depends on", [`green-tea-io`, `green-tea-virtio`, and `alloc`]),
  ("Used by", [`green-tea-bare` (re-exported as `virtio_blk`), and through it `green-tea-coop`]),
  ("Tests", [no host tests; the `disk` kernel, on all three architectures]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Blk::new(transport)`], [negotiates, sets up queue 0, reads the capacity],
  [`capacity()`], [the disk's size, in sectors],
  [`read_only()`], [whether the device negotiated `F_RO`],
  [`write_cache()`], [whether it negotiated `F_FLUSH`, so writes may sit in a cache],
  [`read(sector, buf)`], [reads a whole number of sectors],
  [`write(sector, buf)`], [writes a whole number of sectors],
  [`flush()`], [waits until every write so far is on the disk],
  [`id()`], [the serial number: 20 bytes, NUL-padded],
  [`SECTOR_SIZE`], [512],
  [`F_RO`, `F_FLUSH`], [feature bits 5 and 9, the two the driver accepts],
  [`Error`], [`Unaligned`, `OutOfRange`, `ReadOnly`, `Io`, `Unsupported`, `Device`, with a `Display` message each],
)

== Initialization

`Blk::new` calls `negotiate` accepting only `F_RO` and `F_FLUSH`, sets up queue 0, calls `driver_ok`, and reads the capacity, the first 64-bit field of the configuration area, once. It then allocates the `Request` on the heap, so that its address stays put wherever the `Blk` moves.

== Requests

Every request is one descriptor chain: a header the device reads, the data, and a status byte the device writes.

#bytefield(caption: "A virtio-blk request as a chain of three buffers",
  ("type", "u32", 4), ("reserved", "u32", 4), ("sector", "u64", 8), ("data", "n × 512 bytes", 10), ("status", "u8", 4),
)

The first three fields are the 16-byte `Header`, in one descriptor. The data is the caller's buffer, in a second, and the status byte sits right after the header in the driver's 24-byte `Request`, in a third. A flush has no data, and its chain has two descriptors.

#dtable(columns: (auto, auto, auto, 1fr), ("Request", "Type", "Data", "Direction of the data"),
  [`T_IN`], [0], [the sectors], [the device writes it],
  [`T_OUT`], [1], [the sectors], [the device reads it],
  [`T_FLUSH`], [4], [none], [],
  [`T_GET_ID`], [8], [20 bytes], [the device writes it],
)

The status byte is `S_OK` (0), `S_IOERR` (1) or `S_UNSUPP` (2). The driver starts it at `S_PENDING`, `0xff`, a value the device doesn't send, so that a status left unwritten reads as an error.

`read` and `write` first run `check`, which requires a non-zero whole number of sectors and uses `checked_add`, so that a sector number near `u64::MAX` can't wrap past the capacity. `write` first refuses a read-only disk, and `flush` on a disk without `F_FLUSH` succeeds at once, with no request.


`request` is where they meet:

#srcfile("drivers/virtio-blk/src/lib.rs", lines: ((160, 165), (168, 190),), notes: ((169, [The chain: header, data, status. The two driver-owned `Buffer`s are built on the two lines this listing omits.]), (175, [A spin on the used ring. The `_written` count is ignored.]), (180, [`pop_used` has fenced the ring; this one covers the data and the status byte.]), (181, [With one request at a time, the answer has to be for this chain.]),))[```rust
fn request(&mut self, kind: u32, sector: u64, data: Option<Buffer>) -> Result<(), Error> {
    let request = self.request;
    unsafe {
        write_volatile(&raw mut (*request).header, Header { kind, reserved: 0, sector });
        write_volatile(&raw mut (*request).status, S_PENDING);
    }
// …
    let head = match data {
        Some(data) => self.queue.submit(&self.transport, &[header, data, status]),
        None => self.queue.submit(&self.transport, &[header, status]),
    }
    .map_err(|_| Error::Device)?;

    let done = loop {
        if let Some((done, _written)) = self.queue.pop_used() {
            break done;
        }
        core::hint::spin_loop();
    };
    dma_fence(); // pop_used saw the index; now the data and status it covers
    if done != head {
        return Err(Error::Device);
    }
    match unsafe { read_volatile(&raw const (*request).status) } {
        S_OK => Ok(()),
        S_IOERR => Err(Error::Io),
        S_UNSUPP => Err(Error::Unsupported),
        _ => Err(Error::Device),
    }
}
```]

== Exercised by

The `disk` kernel, in `green-tea-coop`. `cargo xtask kernel` attaches a fresh 64-sector image in which every byte is a function of its sector and offset, so a sector read from the wrong place doesn't pass. The test:

+ reads the id, and compares it with the `serial=green-tea-disk` given to QEMU;
+ reads all 64 sectors, 4 at a time, and checks every byte;
+ checks that a read past the end returns `OutOfRange` and a read of 100 bytes returns `Unaligned`;
+ writes sector 3 with the pattern inverted, flushes, and reads sectors 2 to 4 back: the middle one changed, its neighbours didn't.

#output(title: "cargo xtask kernel --kernel disk --target riscv64 --headless")[
```
virtio-blk over MMIO: 64 sectors (32 KiB), id "green-tea-disk", write cache
PASS: read 64 sectors over virtio-MMIO, and wrote one back
```
]

The `x86_64` run passes the same test over PCI, with the recording in @crate-virtio. The test doesn't exercise `Io`, `Unsupported`, `ReadOnly` or `Device`: QEMU's disk gave it no reason to.

== Limits

- One request at a time, polled, with no timeout: a device that never answers hangs the kernel.
- `Blk::new` doesn't set `STATUS_FAILED` when `Queue::new` fails (line 94), and leaves the device part-way through initialization.
- The capacity is read once, so a resized disk isn't noticed.
- The `_written` count in the used ring is ignored (line 175), so a device that reports success after a short read goes undetected.
- `Blk` has no `Drop`: its `Request` is never freed, and the device is never reset.
- The driver negotiates neither `F_SEG_MAX`, `F_SIZE_MAX` nor `F_BLK_SIZE`, and assumes 512-byte sectors and one data segment of any length.

Three buffers per request, and a disk on every architecture. @crate-bare is what puts these drivers on a machine.
