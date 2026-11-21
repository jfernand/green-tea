#import "../lib.typ": *

= green-tea-virtio-blk <crate-virtio-blk>

`green-tea-virtio-blk` drives a virtio disk over any `Transport`: it reads and writes whole 512-byte sectors, flushes the write cache and reads the device's serial number. It makes one request at a time and polls for the answer.

#spec(
  ("Source", [#repo("drivers/virtio-blk/src/lib.rs"), 339 lines]),
  ("Depends on", [`green-tea-io`, `green-tea-virtio`, and `alloc`]),
  ("Used by", [`green-tea-bare` (re-exported as `virtio_blk`), and through it `green-tea-coop`]),
  ("Tests", [5 host tests, against a fake disk in memory; the `disk` kernel, on all three architectures]),
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
  [`Error`], [`Unaligned`, `OutOfRange`, `TooLong`, `ReadOnly`, `Io`, `Unsupported`, `Device`, with a `Display` message each],
)

== Initialization

`Blk::new` calls `negotiate` accepting only `F_RO` and `F_FLUSH`, sets up queue 0 (marking the device failed if that doesn't work), calls `driver_ok`, and reads the capacity, the first 64-bit field of the configuration area, once. It then allocates the `Request` on the heap, so that its address stays put wherever the `Blk` moves.

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

`read` and `write` first run `check`, which requires a non-zero whole number of sectors under 4 GiB, returning `TooLong` otherwise, and uses `checked_add`, so that a sector number near `u64::MAX` can't wrap past the capacity. `read` then compares the bytes the device says it wrote with the buffer plus the status byte, and a short count is `Device`. `write` first refuses a read-only disk, and `flush` on a disk without `F_FLUSH` succeeds at once, with no request.


`request` is where they meet:

#srcfile("drivers/virtio-blk/src/lib.rs", lines: ((173, 177), (180, 202),), notes: ((181, [The chain: header, data, status. The two driver-owned `Buffer`s are built on the two lines this listing omits.]), (187, [A spin on the used ring, keeping the count of bytes the device wrote.]), (192, [`pop_used` has fenced the ring; this one covers the data and the status byte.]), (193, [With one request at a time, the answer has to be for this chain.]), (197, [`read` compares the count with the buffer, and a short one is `Device`.]),))[```rust
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

    let (done, written) = loop {
        if let Some(used) = self.queue.pop_used() {
            break used;
        }
        core::hint::spin_loop();
    };
    dma_fence(); // pop_used saw the index; now the data and status it covers
    if done != head {
        return Err(Error::Device);
    }
    match unsafe { read_volatile(&raw const (*request).status) } {
        S_OK => Ok(written),
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

The `x86_64` run passes the same test over PCI, with the recording in @crate-virtio. The kernel test doesn't exercise `Io`, `Unsupported`, `ReadOnly` or `Device`: QEMU's disk gave it no reason to. Five host tests use a fake disk that answers on `notify`: a read returns the device's bytes, a failed queue marks the device failed, a short read reported as OK is `Device`, a write needs only its status byte back, and a 4 GiB read is `TooLong` before the device hears of it.

#output(title: "cargo test -p green-tea-virtio-blk")[
```
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

== Limits

- One request at a time, polled, with no timeout: a device that never answers hangs the kernel.
- The capacity is read once, so a resized disk isn't noticed.
- Only `read` checks the used ring's count. `id` ignores it, so a device that writes fewer than 20 bytes and reports success goes undetected.
- `Blk` has no `Drop`: its `Request` is never freed, and the device is never reset.
- The driver negotiates neither `F_SEG_MAX`, `F_SIZE_MAX` nor `F_BLK_SIZE`, and assumes 512-byte sectors and one data segment of any length.

Three buffers per request, and a disk on every architecture. @crate-multiboot2 starts the drivers a physical PC needs.
