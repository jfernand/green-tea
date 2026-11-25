//! What a kernel allocates for itself with, in two layers.
//!
//! - [`frames`]: page frames, handed out in blocks of 1, 2, 4, ... 1,024 pages by a binary
//!   buddy system. What it knows about a frame is in a table kept apart from the frames,
//!   so it never reads or writes a free page.
//! - [`slab`]: caches of objects of one type, after Bonwick's slab allocator. A cache
//!   takes one page at a time from the layer below, cuts it into objects, constructs
//!   them once, and hands them out and takes them back constructed.
//!
//! green-tea-fit's `Buddy` is the same buddy system over bytes, with its records inside
//! the blocks. green-tea-bare makes one `Frames` over the RAM after the kernel, and takes
//! its task records from a `Cache`.

#![no_std]

pub mod frames;
pub mod slab;

/// The size of a page frame.
pub const PAGE: usize = 4096;

/// Where a slab cache gets its pages, and gives them back.
///
/// # Safety
/// `take_page` must return only nonzero, `PAGE`-aligned ranges of writable memory at least
/// `PAGE` bytes long, exclusively owned by this source until returned with `give_page`.
/// Pages returned by this source must not overlap one another or memory owned by anything
/// else.
pub unsafe trait PageSource {
    /// The address of a page nobody else has, or `None` if there is none.
    fn take_page(&mut self) -> Option<usize>;

    /// Give back a page that `take_page` returned.
    ///
    /// # Safety
    /// The page must have come from `take_page` on this source, and not have been given
    /// back since.
    unsafe fn give_page(&mut self, page: usize);
}
