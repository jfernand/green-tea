//! The ramfb display: a framebuffer in ordinary guest RAM. The driver's whole job is to tell
//! QEMU where it is and what shape it has, by writing a config to the fw_cfg file
//! `etc/ramfb`. After that, QEMU shows whatever is in that memory on every refresh.

#![no_std]

use green_tea_fw_cfg::{FwCfg, FwCfgPort};
use core::ptr::write_volatile;

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;

/// XRGB8888: one little-endian u32 per pixel, 0x00RRGGBB. DRM's fourcc code "XR24".
const FOURCC_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

#[repr(C, align(4096))]
struct Pixels([u32; WIDTH * HEIGHT]);

/// Zero-initialized, so it lives in .bss. Memory is mapped one to one (by an identity map
/// on riscv64 and x86_64), so its address is also the physical address QEMU needs.
static mut PIXELS: Pixels = Pixels([0; WIDTH * HEIGHT]);

/// Handle to the configured display: just a pointer to the one set of pixels, which every
/// copy, and every call to `init`, shares. It takes no lock, so keeping two CPUs from
/// drawing at once is the caller's job.
#[derive(Clone, Copy)]
pub struct Framebuffer {
    pixels: *mut u32,
}

impl Framebuffer {
    /// Point QEMU at the framebuffer. Call once.
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

    /// Fill a rectangle, clipped to the screen.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x_end = (x + w).min(WIDTH);
        let y_end = (y + h).min(HEIGHT);
        for row in y.min(HEIGHT)..y_end {
            for col in x.min(WIDTH)..x_end {
                // Volatile: nothing in the program reads these, QEMU does.
                unsafe { write_volatile(self.pixels.add(row * WIDTH + col), color) };
            }
        }
    }

    /// A `thickness`-pixel outline just inside the rectangle. Thicker than the rectangle,
    /// it fills it.
    pub fn outline(&mut self, x: usize, y: usize, w: usize, h: usize, thickness: usize, color: u32) {
        let (across, down) = (thickness.min(w), thickness.min(h));
        self.fill_rect(x, y, w, down, color);
        self.fill_rect(x, y + h - down, w, down, color);
        self.fill_rect(x, y, across, h, color);
        self.fill_rect(x + w - across, y, across, h, color);
    }

    /// Draw `n` in decimal with a 3×5 pixel font, each font pixel `scale`×`scale`.
    /// Returns the width drawn.
    pub fn draw_number(&mut self, x: usize, y: usize, n: usize, scale: usize, color: u32) -> usize {
        let mut digits = [0u8; 20];
        let mut len = 0;
        let mut n = n;
        loop {
            digits[len] = (n % 10) as u8;
            len += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        let advance = 4 * scale; // 3 columns plus a gap
        for (i, &digit) in digits[..len].iter().rev().enumerate() {
            let glyph = FONT[digit as usize];
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..3 {
                    if bits & (0b100 >> col) != 0 {
                        let px = x + i * advance + col * scale;
                        self.fill_rect(px, y + row * scale, scale, scale, color);
                    }
                }
            }
        }
        len * advance
    }
}

/// Digits 0–9, 5 rows of 3 bits each, most significant bit on the left.
const FONT: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b010, 0b010, 0b010], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    const COLOR: u32 = 0x00ff_ffff;

    /// The `(column, row)` of each pixel `draw` paints, in reading order.
    fn painted(draw: impl FnOnce(&mut Framebuffer)) -> Vec<(usize, usize)> {
        let mut pixels = vec![0u32; WIDTH * HEIGHT];
        let mut fb = Framebuffer { pixels: pixels.as_mut_ptr() };
        draw(&mut fb);
        let lit = pixels.iter().enumerate().filter(|&(_, &pixel)| pixel == COLOR);
        lit.map(|(i, _)| (i % WIDTH, i / WIDTH)).collect()
    }

    /// Every pixel of a rectangle, in reading order.
    fn solid(x: usize, y: usize, w: usize, h: usize) -> Vec<(usize, usize)> {
        (y..y + h).flat_map(|row| (x..x + w).map(move |col| (col, row))).collect()
    }

    #[test]
    fn an_outline_leaves_the_middle_alone() {
        let lit = painted(|fb| fb.outline(10, 10, 3, 3, 1, COLOR));
        let ring = solid(10, 10, 3, 3).into_iter().filter(|&p| p != (11, 11)).collect::<Vec<_>>();
        assert_eq!(lit, ring);
    }

    #[test]
    fn an_outline_thicker_than_a_rectangle_at_the_origin_fills_it() {
        assert_eq!(painted(|fb| fb.outline(0, 0, 4, 2, 5, COLOR)), solid(0, 0, 4, 2));
    }

    #[test]
    fn an_outline_thicker_than_its_rectangle_stays_inside_it() {
        assert_eq!(painted(|fb| fb.outline(20, 20, 4, 2, 5, COLOR)), solid(20, 20, 4, 2));
    }
}
