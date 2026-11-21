//! The ramfb display: a framebuffer in ordinary guest RAM. The driver's whole job is to tell
//! QEMU where it is and what shape it has, by writing a config to the fw_cfg file
//! `etc/ramfb`. After that, QEMU shows whatever is in that memory on every refresh.

#![no_std]

use green_tea_framebuffer::Framebuffer;
use green_tea_fw_cfg::{FwCfg, FwCfgPort};

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;

/// XRGB8888: one little-endian u32 per pixel, 0x00RRGGBB. DRM's fourcc code "XR24".
const FOURCC_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

#[repr(C, align(4096))]
struct Pixels([u32; WIDTH * HEIGHT]);

/// Zero-initialized, so it lives in .bss. Memory is mapped one to one (by an identity map
/// on riscv64 and x86_64), so its address is also the physical address QEMU needs.
static mut PIXELS: Pixels = Pixels([0; WIDTH * HEIGHT]);

/// Point QEMU at the framebuffer. Call once; every call shares the one set of pixels.
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
