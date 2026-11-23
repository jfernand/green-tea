#import "../lib.typ": *

= green-tea-framebuffer <crate-framebuffer>

`green-tea-framebuffer` draws on a linear framebuffer: rows of XRGB8888 pixels in memory. It doesn't know who set the display up. @crate-ramfb hands it QEMU's pixels, the `pc` board hands it the mode the firmware chose, and everything drawn after that is the same code.

#spec(
  ("Source", [#repo("drivers/framebuffer/src/lib.rs"), 294 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-ramfb`, `green-tea-bare` (the `screen` module, and the `pc` board's text console)]),
  ("Tests", [9 host tests, on pixels in a vector; the kernels that show a screen draw through it]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Framebuffer::new(pixels, width, height, stride)`], [a handle over pixels someone else set up; `unsafe`, since it trusts the shape],
  [`width()`, `height()`], [the visible size, in pixels],
  [`fill_rect(x, y, w, h, color)`], [fills a rectangle, clipped to the screen],
  [`outline(x, y, w, h, thickness, color)`], [draws a border inside the rectangle, with four `fill_rect`s],
  [`draw_number(x, y, n, scale, color)`], [draws `n` in decimal, and returns the width drawn],
  [`Text::new(fb, x, y, w, h)`], [a text console in a rectangle of the screen, cleared],
  [`Text::put(byte)`], [shows one byte],
)

== Drawing

A pixel is one little-endian `u32`, `0x00RRGGBB`. The handle is a pointer and three sizes, and it is `Copy`. `stride` counts pixels from the start of one row to the start of the next: on ramfb it equals the width, and a firmware's mode may pad its rows, so the two are kept apart. The `pc` board computes it as the bootloader's pitch in bytes, divided by 4.

#srcfile("drivers/framebuffer/src/lib.rs", lines: ((37, 46),), notes: ((38, [Clipping the far edges is enough to keep a write on the screen.]), (40, [A rectangle that starts past the edge gets an empty range.]), (43, [Volatile, because nothing in the program reads these. The display does, and the compiler can't see it.]),))[```rust
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
```]

`outline` first limits `thickness` to the rectangle's width and height, so a border thicker than its rectangle fills it and stays inside.

`draw_number` keeps the digits of `n` in a 20-byte array, enough for any 64-bit value, and draws them from a font of ten glyphs, each 5 rows of 3 bits. One font pixel becomes a `scale` × `scale` square, and each digit advances `4 * scale`: 3 columns and a gap. The returned width includes the gap after the last digit.

== The text console

`Text` turns a rectangle of the screen into a grid of cells. A cell is 12 × 18 pixels: a 5 × 7 font of printable ASCII, drawn at scale 2 with a gap, in light grey on a dark background. It holds its own copy of the handle, and a cursor.

#srcfile("drivers/framebuffer/src/lib.rs", lines: ((130, 153),), notes: ((131, [A rectangle too small for one cell has no grid, and draws nothing.]), (135, [`\n` moves down and keeps the column. The kernel's console sends `\r\n`.]), (137, [Backspace only moves the cursor. Erasing is the caller's: back, a space, back.]), (139, [A long line wraps when the next character arrives, not when the last one fits.]), (152, [Past the bottom it wraps to the top, and the blank row below the newest line shows where the text ends.]),))[```rust
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
```]

The console never reads the screen back, and never scrolls. It writes over the oldest row instead, and each new line clears itself and the one after it. A byte outside the printable range shows as a space.

== Exercised by

Every kernel gets a `Framebuffer` from `boot`, and the ones that show their tasks draw on it through the `Screen` type in `green-tea-bare`. The `pc` board builds a `Text` over the part of the screen the demos don't use, and sends every console byte to it as well as to the serial port. The host tests paint into a vector and compare pixels. They check an outline's ring, one thicker than its rectangle, a stride wider than the screen, and the console's newline, wrapping, backspace and too-small rectangle:

#output(title: "cargo test -p green-tea-framebuffer")[
```
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

On the `pc` board, in QEMU with OVMF, GRUB set up a larger mode than ramfb's:

#output(title: "cargo xtask kernel --board pc --kernel shell --target x86_64 --headless")[
```
green-tea-shell on x86_64 (pc)
framebuffer: 1280x800 XRGB8888
```
]

The recordings are of the serial port, so they show the mode being accepted. They don't show the pixels.

== Limits

- XRGB8888 only. Whoever builds a handle has to have checked the format.
- No lock: the handle is `Copy`, so keeping two CPUs from drawing at once is the caller's job. The `pc` board guards its `Text` with a flag, and drops a character rather than wait.
- `Text` has one size of font and one pair of colors, no scrolling, and no escape sequences.
- `Text` treats bytes one at a time, so a UTF-8 character shows as one space per byte.

Rectangles, digits and a console that never looks back. @crate-ramfb is the first driver to hand it pixels.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: 3290608877a3
// activity: 2026-11-22 | r1 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r1 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
