//! Drawing on a linear framebuffer: rows of XRGB8888 pixels in memory. Whatever set the
//! display up (QEMU's ramfb, or the firmware's mode that a bootloader reports) hands over
//! the address and the shape, and everything drawn after that is the same.

#![no_std]

use core::ptr::write_volatile;

/// Handle to a display: a pointer to its pixels and their shape. It takes no lock, so
/// keeping two CPUs from drawing at once is the caller's job.
#[derive(Clone, Copy)]
pub struct Framebuffer {
    pixels: *mut u32,
    width: usize,
    height: usize,
    /// Pixels from the start of one row to the start of the next: `width` or more.
    stride: usize,
}

impl Framebuffer {
    /// # Safety
    /// `pixels` must be valid for writes of `stride * height` u32s for as long as this
    /// handle and its copies are used, with `stride >= width`.
    pub const unsafe fn new(pixels: *mut u32, width: usize, height: usize, stride: usize) -> Framebuffer {
        Framebuffer { pixels, width, height, stride }
    }

    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    /// Fill a rectangle, clipped to the screen.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        for row in y.min(self.height)..y_end {
            for col in x.min(self.width)..x_end {
                // Volatile: nothing in the program reads these, the display does.
                unsafe { write_volatile(self.pixels.add(row * self.stride + col), color) };
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

    const WIDTH: usize = 640;
    const HEIGHT: usize = 480;
    const COLOR: u32 = 0x00ff_ffff;

    /// The `(column, row)` of each pixel `draw` paints, in reading order.
    fn painted(draw: impl FnOnce(&mut Framebuffer)) -> Vec<(usize, usize)> {
        let mut pixels = vec![0u32; WIDTH * HEIGHT];
        // SAFETY: `pixels` is WIDTH × HEIGHT and outlives `fb`.
        let mut fb = unsafe { Framebuffer::new(pixels.as_mut_ptr(), WIDTH, HEIGHT, WIDTH) };
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

    #[test]
    fn a_stride_wider_than_the_screen_leaves_the_padding_alone() {
        let mut pixels = vec![0u32; 4 * 2];
        // SAFETY: two rows of four pixels, of which three are on the screen.
        let mut fb = unsafe { Framebuffer::new(pixels.as_mut_ptr(), 3, 2, 4) };
        fb.fill_rect(0, 0, 10, 10, COLOR);
        assert_eq!(pixels, [COLOR, COLOR, COLOR, 0, COLOR, COLOR, COLOR, 0]);
    }
}
