#import "../lib.typ": *

= green-tea-ramfb <crate-ramfb>

`green-tea-ramfb` drives QEMU's ramfb display: a framebuffer in ordinary guest RAM. The driver tells QEMU where the pixels are and what shape they have by writing 28 bytes to the fw_cfg file `etc/ramfb`, and from then on QEMU shows that memory on every refresh. The crate also draws: rectangles, outlines and decimal digits.

#spec(
  ("Source", [#repo("drivers/ramfb/src/lib.rs"), 113 lines]),
  ("Depends on", [`green-tea-fw-cfg`]),
  ("Used by", [`green-tea-bare` (`boot`, and the `screen` module)]),
  ("Tests", [no host tests; every kernel calls `init` at boot, and `Screen` draws through the rest]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`WIDTH`, `HEIGHT`], [640 and 480, fixed at compile time],
  [`Framebuffer::init(fw_cfg)`], [points QEMU at the pixels; returns the handle],
  [`fill_rect(x, y, w, h, color)`], [fills a rectangle, clipped to the screen],
  [`outline(x, y, w, h, thickness, color)`], [draws a border inside the rectangle, with four `fill_rect`s],
  [`draw_number(x, y, n, scale, color)`], [draws `n` in decimal, and returns the width drawn],
)

== The configuration

The pixels are a `static mut` of 640 × 480 `u32`s, 1,228,800 bytes, page-aligned and zero-initialized, so they take no space in the ELF. A pixel is XRGB8888: one little-endian `u32`, `0x00RRGGBB`.

#bytefield(caption: "RAMFBCfg, 28 bytes, packed, big-endian",
  ("addr", "u64", 8), ("fourcc", "u32 “XR24”", 4), ("flags", "u32, 0", 4), ("width", "u32, 640", 4), ("height", "u32, 480", 4), ("stride", "u32, 2560", 4),
)

#srcfile("drivers/ramfb/src/lib.rs", lines: ((31, 49),), size: 6.5pt, notes: ((32, [A missing file means QEMU wasn't started with `-device ramfb`, and the message says so.]), (39, [`FOURCC_XRGB8888` is DRM's code, the bytes `XR24` read as a little-endian `u32`. It then goes in big-endian, like every other field.]), (44, [The config is built before the size is compared, but nothing is sent until the check passes.]),))[```rust
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
    Ok(Framebuffer { pixels })
}
```]

The stride is `WIDTH * 4`, 2,560 bytes: rows are contiguous.

== Drawing

`fill_rect` clips to the screen and writes each pixel with a volatile store, because no code in the kernel reads the pixels: QEMU does, and the compiler can't see it.


`draw_number` keeps the digits of `n` in a 20-byte array, enough for any 64-bit value, and draws them from a font of ten glyphs, each 5 rows of 3 bits. One font pixel becomes a `scale` × `scale` square, and each digit advances `4 * scale`: 3 columns and a gap. The returned width includes the gap after the last digit.

== Exercised by

`boot` calls `init` in every kernel and panics if it fails, so a kernel that prints the `ramfb:` line has a configured display. In the headless runs nobody looks at the window, so the recording shows the configuration succeeding, not the pixels. The `Screen` type in `green-tea-bare` draws its columns with `fill_rect`, `outline` and `draw_number`.

#output(title: "cargo xtask kernel --kernel disk --target aarch64 --headless")[
```
green-tea-coop on aarch64
ramfb: 640x480 XRGB8888
PASS: read 64 sectors over virtio-MMIO, and wrote one back
```
]

== Limits

- One size, one format, one framebuffer.
- `outline` doesn't check `thickness` against the rectangle: lines 66 and 68 subtract it from `y + h` and `x + w`, which underflows when it exceeds them.
- The comment on `PIXELS` (lines 19 and 20) says there is no paging. `riscv64` and `x86_64` run with paging on, through an identity map, so the address is still the physical one, for a different reason.
- The comments on `Framebuffer` (lines 23 and 24) say only one handle exists and that the machine has a single hart. The type is `Copy`, `init` can be called again, and the `smp` kernel runs four CPUs. Nothing serializes drawing.

Twenty-eight bytes buy a screen. @crate-pci is how `x86_64` finds its other devices.
