#import "../lib.typ": *

= green-tea-ramfb <crate-ramfb>

`green-tea-ramfb` drives QEMU's ramfb display: a framebuffer in ordinary guest RAM. The driver tells QEMU where the pixels are and what shape they have by writing 28 bytes to the fw_cfg file `etc/ramfb`, and from then on QEMU shows that memory on every refresh. It has one function, `init`, and hands back a @crate-framebuffer handle, which does all the drawing.

#spec(
  ("Source", [#repo("drivers/ramfb/src/lib.rs"), 43 lines]),
  ("Depends on", [`green-tea-fw-cfg`, `green-tea-framebuffer`]),
  ("Used by", [`green-tea-bare` (the `Qemu` trait's `ramfb_display`, on the three QEMU boards)]),
  ("Tests", [no host tests; every kernel on QEMU's boards calls `init` at boot]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`WIDTH`, `HEIGHT`], [640 and 480, fixed at compile time],
  [`init(fw_cfg)`], [points QEMU at the pixels, and returns a `Framebuffer` over them],
)

== The configuration

The pixels are a `static mut` of 640 × 480 `u32`s, 1,228,800 bytes, page-aligned and zero-initialized, so they take no space in the ELF. Memory is mapped one to one, through an identity map on `riscv64` and `x86_64`, so the static's address is also the physical address QEMU needs. A pixel is XRGB8888: one little-endian `u32`, `0x00RRGGBB`.

#bytefield(caption: "RAMFBCfg, 28 bytes, packed, big-endian",
  ("addr", "u64", 8), ("fourcc", "u32 “XR24”", 4), ("flags", "u32, 0", 4), ("width", "u32, 640", 4), ("height", "u32, 480", 4), ("stride", "u32, 2560", 4),
)

#srcfile("drivers/ramfb/src/lib.rs", lines: ((24, 43),), size: 6.5pt, notes: ((25, [A missing file means QEMU wasn't started with `-device ramfb`, and the message says so.]), (32, [`FOURCC_XRGB8888` is DRM's code, the bytes `XR24` read as a little-endian `u32`. It then goes in big-endian, like every other field.]), (37, [The config is built before the size is compared, but nothing is sent until the check passes.]), (42, [The stride here counts pixels. The 2,560 in the config is the same row in bytes.]),))[```rust
pub fn init(fw_cfg: &FwCfg<impl FwCfgPort>) -> Result<Framebuffer, &'static str> {
    let (key, size) = fw_cfg.find_file("etc/ramfb").ok_or("no etc/ramfb (run qemu with -device ramfb)")?;
    let pixels = (&raw mut PIXELS).cast::<u32>();

    // struct RAMFBCfg { u64 addr; u32 fourcc, flags, width, height, stride; }, packed,
    // big-endian.
    let mut config = [0u8; 28];
    config[0..8].copy_from_slice(&(pixels as u64).to_be_bytes());
    config[8..12].copy_from_slice(&FOURCC_XRGB8888.to_be_bytes());
    config[12..16].copy_from_slice(&0u32.to_be_bytes());
    config[16..20].copy_from_slice(&(WIDTH as u32).to_be_bytes());
    config[20..24].copy_from_slice(&(HEIGHT as u32).to_be_bytes());
    config[24..28].copy_from_slice(&(WIDTH as u32 * 4).to_be_bytes());
    if size as usize != config.len() {
        return Err("etc/ramfb has an unexpected size");
    }
    fw_cfg.dma_write(key, &config)?;
    // SAFETY: PIXELS is a static of WIDTH × HEIGHT, and rows are WIDTH apart.
    Ok(unsafe { Framebuffer::new(pixels, WIDTH, HEIGHT, WIDTH) })
}
```]

The stride is `WIDTH * 4`, 2,560 bytes: rows are contiguous.

== Exercised by

`boot` reaches `init` in every kernel on QEMU's boards and panics if it fails, so a kernel that prints the `ramfb:` line has a configured display. In the headless runs nobody looks at the window, so the recording shows the configuration succeeding, not the pixels.

#output(title: "cargo xtask kernel --kernel disk --target aarch64 --headless")[
```
green-tea-coop on aarch64
ramfb: 640x480 XRGB8888
PASS: read 64 sectors over virtio-MMIO, and wrote one back
```
]

== Limits

- One size, one format, one framebuffer.
- `init` can be called again, and every call shares the one set of pixels. The `Framebuffer` is `Copy` and takes no lock, so keeping two CPUs from drawing at once is the caller's job.
- It exists only under QEMU: the `pc` board gets its screen from the bootloader instead, through @crate-multiboot2.

Twenty-eight bytes buy a screen. @crate-pci is how `x86_64` finds its other devices.
