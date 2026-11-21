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

/// A text console in a rectangle of the screen. It never reads the screen back: at the
/// bottom it wraps to the top, and each new line clears itself and the one after it, so
/// the newest text is always just above a blank line.
pub struct Text {
    fb: Framebuffer,
    x: usize,
    y: usize,
    columns: usize,
    rows: usize,
    column: usize,
    row: usize,
}

const SCALE: usize = 2;
const CELL_W: usize = 6 * SCALE;
const CELL_H: usize = 9 * SCALE;
const TEXT_FG: u32 = 0x00e8_e8e8;
const TEXT_BG: u32 = 0x0010_1418;

impl Text {
    /// A console in the `w` × `h` pixels at (`x`, `y`), cleared.
    pub fn new(mut fb: Framebuffer, x: usize, y: usize, w: usize, h: usize) -> Text {
        fb.fill_rect(x, y, w, h, TEXT_BG);
        Text { fb, x, y, columns: w / CELL_W, rows: h / CELL_H, column: 0, row: 0 }
    }

    /// Show one byte. `\n` starts a new line, `\r` goes back to its start, backspace (8 or
    /// 0x7f) moves back one cell, and anything else unprintable shows as a space.
    pub fn put(&mut self, byte: u8) {
        if self.columns == 0 || self.rows == 0 {
            return;
        }
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.column = 0,
            8 | 0x7f => self.column = self.column.saturating_sub(1),
            _ => {
                if self.column == self.columns {
                    self.newline();
                    self.column = 0;
                }
                self.glyph(self.column, self.row, byte);
                self.column += 1;
            }
        }
    }

    fn newline(&mut self) {
        self.row = (self.row + 1) % self.rows;
        self.clear_row(self.row);
        self.clear_row((self.row + 1) % self.rows);
    }

    fn clear_row(&mut self, row: usize) {
        self.fb.fill_rect(self.x, self.y + row * CELL_H, self.columns * CELL_W, CELL_H, TEXT_BG);
    }

    fn glyph(&mut self, column: usize, row: usize, byte: u8) {
        let (x, y) = (self.x + column * CELL_W, self.y + row * CELL_H);
        self.fb.fill_rect(x, y, CELL_W, CELL_H, TEXT_BG);
        let index = if (0x20..0x7f).contains(&byte) { (byte - 0x20) as usize } else { 0 };
        for (c, bits) in TEXT_FONT[index].iter().enumerate() {
            for r in 0..7 {
                if bits >> r & 1 != 0 {
                    self.fb.fill_rect(x + c * SCALE, y + r * SCALE, SCALE, SCALE, TEXT_FG);
                }
            }
        }
    }
}

/// Printable ASCII in 5×7: five columns each, bit 0 at the top.
#[rustfmt::skip]
const TEXT_FONT: [[u8; 5]; 95] = [
    [0x00,0x00,0x00,0x00,0x00],[0x00,0x00,0x5f,0x00,0x00],[0x00,0x07,0x00,0x07,0x00],[0x14,0x7f,0x14,0x7f,0x14],
    [0x24,0x2a,0x7f,0x2a,0x12],[0x23,0x13,0x08,0x64,0x62],[0x36,0x49,0x55,0x22,0x50],[0x00,0x05,0x03,0x00,0x00],
    [0x00,0x1c,0x22,0x41,0x00],[0x00,0x41,0x22,0x1c,0x00],[0x14,0x08,0x3e,0x08,0x14],[0x08,0x08,0x3e,0x08,0x08],
    [0x00,0x50,0x30,0x00,0x00],[0x08,0x08,0x08,0x08,0x08],[0x00,0x60,0x60,0x00,0x00],[0x20,0x10,0x08,0x04,0x02],
    [0x3e,0x51,0x49,0x45,0x3e],[0x00,0x42,0x7f,0x40,0x00],[0x42,0x61,0x51,0x49,0x46],[0x21,0x41,0x45,0x4b,0x31],
    [0x18,0x14,0x12,0x7f,0x10],[0x27,0x45,0x45,0x45,0x39],[0x3c,0x4a,0x49,0x49,0x30],[0x01,0x71,0x09,0x05,0x03],
    [0x36,0x49,0x49,0x49,0x36],[0x06,0x49,0x49,0x29,0x1e],[0x00,0x36,0x36,0x00,0x00],[0x00,0x56,0x36,0x00,0x00],
    [0x08,0x14,0x22,0x41,0x00],[0x14,0x14,0x14,0x14,0x14],[0x00,0x41,0x22,0x14,0x08],[0x02,0x01,0x51,0x09,0x06],
    [0x32,0x49,0x79,0x41,0x3e],[0x7e,0x11,0x11,0x11,0x7e],[0x7f,0x49,0x49,0x49,0x36],[0x3e,0x41,0x41,0x41,0x22],
    [0x7f,0x41,0x41,0x22,0x1c],[0x7f,0x49,0x49,0x49,0x41],[0x7f,0x09,0x09,0x09,0x01],[0x3e,0x41,0x49,0x49,0x7a],
    [0x7f,0x08,0x08,0x08,0x7f],[0x00,0x41,0x7f,0x41,0x00],[0x20,0x40,0x41,0x3f,0x01],[0x7f,0x08,0x14,0x22,0x41],
    [0x7f,0x40,0x40,0x40,0x40],[0x7f,0x02,0x0c,0x02,0x7f],[0x7f,0x04,0x08,0x10,0x7f],[0x3e,0x41,0x41,0x41,0x3e],
    [0x7f,0x09,0x09,0x09,0x06],[0x3e,0x41,0x51,0x21,0x5e],[0x7f,0x09,0x19,0x29,0x46],[0x46,0x49,0x49,0x49,0x31],
    [0x01,0x01,0x7f,0x01,0x01],[0x3f,0x40,0x40,0x40,0x3f],[0x1f,0x20,0x40,0x20,0x1f],[0x3f,0x40,0x38,0x40,0x3f],
    [0x63,0x14,0x08,0x14,0x63],[0x07,0x08,0x70,0x08,0x07],[0x61,0x51,0x49,0x45,0x43],[0x00,0x7f,0x41,0x41,0x00],
    [0x02,0x04,0x08,0x10,0x20],[0x00,0x41,0x41,0x7f,0x00],[0x04,0x02,0x01,0x02,0x04],[0x40,0x40,0x40,0x40,0x40],
    [0x00,0x01,0x02,0x04,0x00],[0x20,0x54,0x54,0x54,0x78],[0x7f,0x48,0x44,0x44,0x38],[0x38,0x44,0x44,0x44,0x20],
    [0x38,0x44,0x44,0x48,0x7f],[0x38,0x54,0x54,0x54,0x18],[0x08,0x7e,0x09,0x01,0x02],[0x0c,0x52,0x52,0x52,0x3e],
    [0x7f,0x08,0x04,0x04,0x78],[0x00,0x44,0x7d,0x40,0x00],[0x20,0x40,0x44,0x3d,0x00],[0x7f,0x10,0x28,0x44,0x00],
    [0x00,0x41,0x7f,0x40,0x00],[0x7c,0x04,0x18,0x04,0x78],[0x7c,0x08,0x04,0x04,0x78],[0x38,0x44,0x44,0x44,0x38],
    [0x7c,0x14,0x14,0x14,0x08],[0x08,0x14,0x14,0x18,0x7c],[0x7c,0x08,0x04,0x04,0x08],[0x48,0x54,0x54,0x54,0x20],
    [0x04,0x3f,0x44,0x40,0x20],[0x3c,0x40,0x40,0x20,0x7c],[0x1c,0x20,0x40,0x20,0x1c],[0x3c,0x40,0x30,0x40,0x3c],
    [0x44,0x28,0x10,0x28,0x44],[0x0c,0x50,0x50,0x50,0x3c],[0x44,0x64,0x54,0x4c,0x44],[0x00,0x08,0x36,0x41,0x00],
    [0x00,0x00,0x7f,0x00,0x00],[0x00,0x41,0x36,0x08,0x00],[0x10,0x08,0x08,0x10,0x08],
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

    /// The rows of a small console's screen that have any text on them, after `typed`.
    fn text_rows(columns: usize, rows: usize, typed: &[u8]) -> Vec<usize> {
        let (w, h) = (columns * CELL_W, rows * CELL_H);
        let mut pixels = vec![0u32; w * h];
        // SAFETY: `pixels` is w × h and outlives `fb`.
        let fb = unsafe { Framebuffer::new(pixels.as_mut_ptr(), w, h, w) };
        let mut text = Text::new(fb, 0, 0, w, h);
        typed.iter().for_each(|&b| text.put(b));
        (0..rows).filter(|r| pixels[r * CELL_H * w..(r + 1) * CELL_H * w].contains(&TEXT_FG)).collect()
    }

    #[test]
    fn text_starts_a_new_row_on_a_newline() {
        assert_eq!(text_rows(10, 4, b"a\r\nb"), [0, 1]);
    }

    #[test]
    fn a_long_line_wraps() {
        assert_eq!(text_rows(3, 4, b"abcd"), [0, 1]);
    }

    #[test]
    fn past_the_bottom_it_wraps_and_keeps_a_blank_row_below() {
        assert_eq!(text_rows(4, 3, b"a\r\nb\r\nc\r\nd"), [0, 2]);
    }

    #[test]
    fn backspace_then_a_space_erases() {
        assert_eq!(text_rows(4, 2, b"a\x08 "), Vec::<usize>::new());
    }

    #[test]
    fn a_console_too_small_for_one_cell_draws_nothing() {
        let mut pixels = vec![0u32; 4];
        // SAFETY: two by two, and `pixels` outlives `fb`.
        let fb = unsafe { Framebuffer::new(pixels.as_mut_ptr(), 2, 2, 2) };
        let mut text = Text::new(fb, 0, 0, 2, 2);
        text.put(b'a');
        assert!(!pixels.contains(&TEXT_FG));
    }
}
