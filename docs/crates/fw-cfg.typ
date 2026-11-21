#import "../lib.typ": *

= green-tea-fw-cfg <crate-fw-cfg>

`green-tea-fw-cfg` drives QEMU's firmware configuration device, fw_cfg: a list of numbered items, some fixed and some named files like `etc/ramfb`. A driver selects an item and reads its bytes from a data register, or writes to it with a DMA request. Every kernel on QEMU's boards uses it at boot to find the display, and to learn which test to run. The `pc` board has no fw_cfg.

#spec(
  ("Source", [#repo("drivers/fw-cfg/src/lib.rs"), 245 lines]),
  ("Depends on", [`green-tea-io`]),
  ("Used by", [`green-tea-ramfb`, `green-tea-bare`]),
  ("Tests", [2 host tests, against a fake port; every kernel on QEMU's boards boots through it, on all three architectures]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it is"),
  [`FwCfgPort`], [a trait: `select`, `read_u8` and `start_dma`, the registers however the machine reaches them],
  [`Mmio(pub usize)`], [the registers memory-mapped, at `0x1010_0000` on `riscv64` `virt`],
  [`IoPorts`], [the registers on I/O ports `0x510` to `0x518`; `x86_64` only],
  [`FwCfg<P>`], [the protocol over a port: `probe`, `for_each_file`, `find_file`, `read_file`, `dma_write`],
  [`File`], [one directory entry: `name`, `size`, `select`, and a `name()` method that returns the name up to its NUL as a `&str`, or `"?"` if it isn't UTF-8],
)

== The ports

The protocol is the same on every machine, and the registers differ in where they are and in byte order. On a little-endian CPU, `Mmio` swaps with `to_be` before each volatile store. Both ports write the high half of the DMA address first, because the write to the low half starts the transfer.

#dtable(columns: (auto, auto, auto), ("Register", "`Mmio`", "`IoPorts`"),
  [selector, 16 bits], [`+0x08`, big-endian], [`0x510`, little-endian],
  [data, 8 bits], [`+0x00`], [`0x511`],
  [DMA address, high half], [`+0x10`, big-endian], [`0x514`, big-endian],
  [DMA address, low half (starts the transfer)], [`+0x14`, big-endian], [`0x518`, big-endian],
)


`IoPorts::select` is the odd one out: it writes the key with `outw` unswapped, as the port selector is little-endian. The `q35` board also uses `IoPorts` directly: `start_cpu` reads item 5, the number of CPUs QEMU was started with, and `ram_end` reads item 3, the size of RAM.

== Probing

`probe` selects item 0 and compares its four bytes with `QEMU`, then selects item 1, the feature bitmap, and requires bit 1 (`ID_DMA`). The bitmap is one of the few little-endian items. Each failure returns a message, and `green-tea-bare`'s `boot` panics with it, since a kernel on these boards can't turn on its display without DMA.

== The file directory

Item `0x19` is the file directory: a big-endian count, then that many 64-byte entries.

#bytefield(caption: "One entry of the fw_cfg file directory, 64 bytes, big-endian",
  ("size", "u32", 2), ("select", "u16", 2), ("reserved", "u16", 2), ("name", "[u8; 56], NUL-padded", 6),
)


`for_each_file` reads the count and calls its closure with a `File` for each entry. `find_file` walks the directory with it and returns the selector and size of the first match. `read_file` finds a file, selects it and reads at most `buf.len()` bytes, returning how many, or `None` if there's no such file. A directory walk leaves the device's selection on item `0x19`, so a callback that selects another item derails the walk. `find_file`'s callback doesn't.

== DMA writes

Writing an item needs the DMA interface. The request is a 16-byte structure in guest memory, all big-endian, aligned to 16 bytes:

#bytefield(caption: "DmaAccess, 16 bytes, big-endian",
  ("control", "u32", 4), ("length", "u32", 4), ("address", "u64", 8),
)

#bytefield(caption: "The control word of a write",
  ("key", "bits 31–16", 8), ("unused", "bits 15–5", 4), ("WRITE", "bit 4", 2), ("SELECT", "bit 3", 2), ("unused", "bits 2–1", 2), ("ERROR", "bit 0", 2),
)

#srcfile("drivers/fw-cfg/src/lib.rs", lines: ((133, 154),), notes: ((140, [The request is on this function's stack. Memory is mapped one to one, so its address is also the physical address QEMU needs.]), (141, [The request and the data have to be in memory before the register write that starts the transfer.]), (134, [A length that doesn't fit the request's 32 bits is refused before the device hears of it.]), (146, [Volatile, because QEMU writes this field and nothing in the program does.]),))[```rust
pub fn dma_write(&self, key: u16, data: &[u8]) -> Result<(), &'static str> {
    let length = u32::try_from(data.len()).map_err(|_| "fw_cfg DMA write of 4 GiB or more")?;
    let mut access = DmaAccess {
        control: ((u32::from(key) << 16) | DMA_SELECT | DMA_WRITE).to_be(),
        length: length.to_be(),
        address: (data.as_ptr() as u64).to_be(),
    };
    let address = &raw mut access as u64;
    dma_fence(); // the request and the data are in memory before QEMU looks
    self.0.start_dma(address);
    // QEMU clears the control field when it's done, leaving only the error bit if it
    // failed.
    loop {
        let control = u32::from_be(unsafe { read_volatile(&raw const access.control) });
        if control & DMA_ERROR != 0 {
            return Err("fw_cfg DMA write failed");
        }
        if control == 0 {
            return Ok(());
        }
    }
}
```]

QEMU reads the request, does the write, and clears `control`, leaving only the error bit if it failed. The loop spins on that word, and the stack frame stays alive until it has seen the answer.

== Exercised by

On QEMU's boards, `boot` reaches the `Qemu` trait's `ramfb_display`, which calls `probe`, prints the directory with `for_each_file`, and has `green-tea-ramfb` find `etc/ramfb` with `find_file` and write it with `dma_write`. `green-tea-coop` then asks the board for the test name, and `fw_cfg_test_name` calls `read_file` on `opt/green-tea/test`, a file `cargo xtask kernel` creates with `-fw_cfg`. Its size is the length of the test name: 4 bytes, for `disk`. Two host tests drive `dma_write` against a fake port: one write reaches the device, and a 4 GiB slice is refused without starting a transfer.

#output(title: "cargo xtask kernel --kernel disk --target riscv64 --headless")[
```
green-tea-coop on riscv64
fw_cfg files:
  0x0020      0 bios-geometry
  0x0021      0 bootorder
  0x0022     36 etc/acpi/rsdp
  0x0023 131072 etc/acpi/tables
  0x0024      4 etc/boot-fail-wait
  0x0025     28 etc/ramfb
  0x0026   1792 etc/table-loader
  0x0027      4 opt/green-tea/test
  0x0028  28672 vgaroms/vgabios-ramfb.bin
ramfb: 640x480 XRGB8888
```
]

The selectors depend on the machine: `etc/ramfb` is `0x0026` on `x86_64`, where the directory also holds `etc/e820` and `genroms/pvh.bin`.

== Limits

- Only DMA *writes*. Reads go through the data register one byte at a time.
- `dma_write` spins with no timeout.
- `read_file` truncates to the buffer without saying so: a result equal to `buf.len()` doesn't tell a complete read from a cut one.
- Every lookup rereads the directory from the start.

A numbered list with a DMA door. @crate-ramfb goes through that door once, and @crate-framebuffer draws on what it sets up.
