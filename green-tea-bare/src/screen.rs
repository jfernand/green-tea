//! The demos' screen: a status bar on top, and a column per worker below it, with a bar
//! that grows as the worker makes progress. The column of the task that's about to run is
//! outlined in white. Task ids follow creation order: 0 is the main task, and the worker
//! in column `c` has id `c + 1`.

use crate::ramfb::{Framebuffer, HEIGHT, WIDTH};

pub const COLUMNS: usize = 5;
pub const COLORS: [u32; COLUMNS] = [0xe0605a, 0xe8b44a, 0x6cc070, 0x4fa3e0, 0xb07ce0];

const STATUS_H: usize = 60;
const COL_W: usize = WIDTH / COLUMNS;
const BORDER: usize = 4;
const INSET: usize = 12;
const LABEL_H: usize = 20;

/// How tall a bar can grow, in pixels: from the bottom inset up to just below the label.
pub const BAR_MAX: usize = HEIGHT - STATUS_H - 3 * INSET - LABEL_H;

pub const BACKGROUND: u32 = 0x202428;
const STATUS_BG: u32 = 0x30363c;
const DONE_BG: u32 = 0x2e5a3a;
const IDLE_BORDER: u32 = 0x3c444c;
const ACTIVE_BORDER: u32 = 0xffffff;
const TEXT: u32 = 0xe8e8e8;

/// The whole screen, owned by whoever runs the scheduler: it tracks which column is
/// highlighted, so there must be only one. Workers draw through a [`Painter`].
pub struct Screen {
    fb: Framebuffer,
    highlighted: Option<usize>,
}

/// What a worker draws with: its bar and label. Copies are fine, since each worker draws
/// only in its own column.
#[derive(Clone, Copy)]
pub struct Painter {
    fb: Framebuffer,
}

impl Screen {
    /// Clear the screen and draw the empty columns.
    pub fn new(mut fb: Framebuffer) -> Screen {
        fb.fill_rect(0, 0, WIDTH, HEIGHT, BACKGROUND);
        fb.fill_rect(0, 0, WIDTH, STATUS_H, STATUS_BG);
        let mut screen = Screen { fb, highlighted: None };
        for column in 0..COLUMNS {
            screen.outline(column, IDLE_BORDER);
        }
        screen
    }

    pub fn painter(&self) -> Painter {
        Painter { fb: self.fb }
    }

    /// Task `id` is about to run: move the highlight to its column, and show `switches`
    /// and `id` in the status bar.
    pub fn switch_to(&mut self, id: usize, switches: usize) {
        let column = id.checked_sub(1); // the main task has no column
        if let Some(previous) = core::mem::replace(&mut self.highlighted, column) {
            self.outline(previous, IDLE_BORDER);
        }
        if let Some(column) = column {
            self.outline(column, ACTIVE_BORDER);
        }
        self.status(STATUS_BG, switches);
        self.fb.fill_rect(WIDTH - 60, 15, 36, 30, COLORS[(id + COLUMNS - 1) % COLUMNS]);
        self.fb.draw_number(WIDTH - 54, 20, id, 4, BACKGROUND);
    }

    /// No task is runnable: drop the highlight, and show an empty box for the task id.
    pub fn idle(&mut self, switches: usize) {
        if let Some(previous) = self.highlighted.take() {
            self.outline(previous, IDLE_BORDER);
        }
        self.status(STATUS_BG, switches);
        self.fb.outline(WIDTH - 60, 15, 36, 30, 2, IDLE_BORDER);
    }

    /// Everything is done: drop the highlight and turn the status bar green.
    pub fn finish(&mut self, switches: usize) {
        if let Some(previous) = self.highlighted.take() {
            self.outline(previous, IDLE_BORDER);
        }
        self.status(DONE_BG, switches);
    }

    fn outline(&mut self, column: usize, color: u32) {
        self.fb.outline(column * COL_W, STATUS_H, COL_W, HEIGHT - STATUS_H, BORDER, color);
    }

    fn status(&mut self, background: u32, switches: usize) {
        self.fb.fill_rect(0, 0, WIDTH, STATUS_H, background);
        self.fb.draw_number(16, 15, switches, 6, TEXT);
    }
}

impl Painter {
    /// Fill `column`'s bar from `from` to `to` pixels above its bottom.
    pub fn bar(&mut self, column: usize, from: usize, to: usize, color: u32) {
        let x = column * COL_W + INSET;
        let bottom = HEIGHT - INSET;
        let (from, to) = (from.min(BAR_MAX), to.min(BAR_MAX));
        self.fb.fill_rect(x, bottom - to, COL_W - 2 * INSET, to.saturating_sub(from), color);
    }

    /// A number at the top of `column`: how far its worker has got.
    pub fn label(&mut self, column: usize, n: usize) {
        let x = column * COL_W + INSET;
        self.fb.fill_rect(x, STATUS_H + INSET, COL_W - 2 * INSET, LABEL_H, BACKGROUND);
        self.fb.draw_number(x, STATUS_H + INSET, n, 4, TEXT);
    }
}
