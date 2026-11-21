//! Blocks and pages: which pages a block is on, and which pages of a free block hold nothing.
//!
//! The allocators of this crate work in bytes. The memory under them comes in pages, and a
//! kernel can take a page back only whole. A free block keeps four words that matter, its
//! header and its two links at the start and its footer at the end; everything between
//! them is unused until the block is allocated again. [`idle`] is the whole pages in that
//! middle: what an allocator can give back to whoever is underneath it without losing
//! anything it will read. [`Fit`](crate::Fit) and [`Tlsf`](crate::tlsf::Tlsf) lay a free
//! block out the same way, so it serves both.

use crate::{Block, WORD};
use core::ops::Range;

/// The whole pages inside the free block `block` that hold neither its header, nor its
/// two links, nor its footer. `page` is the page size, a power of two. The range is empty
/// if the block has no such page.
pub fn idle(block: Block, page: usize) -> Range<usize> {
    debug_assert!(page.is_power_of_two() && !block.used);
    // The header and the links are the block's first three words; the footer is its last.
    let start = (block.start + 3 * WORD).next_multiple_of(page);
    let end = (block.start + block.size - WORD) & !(page - 1);
    start..end.max(start)
}

/// The idle pages of every free block among `blocks`, lowest first, leaving out the blocks
/// that have none.
pub fn idle_ranges(blocks: impl Iterator<Item = Block>, page: usize) -> impl Iterator<Item = Range<usize>> {
    blocks
        .filter(|block| !block.used)
        .map(move |block| idle(block, page))
        .filter(|range| !range.is_empty())
}

/// How many pages the bytes `start..end` are on.
pub fn spanned(start: usize, end: usize, page: usize) -> usize {
    debug_assert!(page.is_power_of_two());
    if end <= start { 0 } else { (end - 1) / page - start / page + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlsf::Tlsf;
    use crate::trace::Random;
    use crate::{Fit, Order, Policy};
    use alloc::vec;
    use alloc::vec::Vec;

    const PAGE: usize = 256; // small, so that a region of a few KiB has many

    fn free(start: usize, size: usize) -> Block {
        Block { start, size, used: false }
    }

    // A block's header is at an address that is 8 more than a multiple of 16, and its size
    // is a multiple of 16: the tests' blocks are placed as real ones are.

    #[test]
    fn a_block_smaller_than_a_page_has_no_idle_page() {
        assert!(idle(free(8, 240), PAGE).is_empty());
        assert!(idle(free(1000, 32), PAGE).is_empty());
        // Nor has one that is on three pages and fills only the middle one.
        assert!(idle(free(136, 368), PAGE).is_empty());
        assert_eq!(idle(free(136, 512), PAGE), 256..512);
    }

    #[test]
    fn the_pages_with_the_header_the_links_and_the_footer_are_kept() {
        // Header at 248, the last word of the first page; links at 256 and 264, in the
        // second. The footer is the word at 1024, the first of the fifth page.
        assert_eq!(idle(free(248, 784), PAGE), 512..1024);
        // The same block two words longer: nothing changes, the footer is still in the fifth.
        assert_eq!(idle(free(248, 800), PAGE), 512..1024);
        // Header and both links in the first page, ending where the second begins.
        assert_eq!(idle(free(232, 800), PAGE), 256..1024);
        // The second link is the first word of the second page.
        assert_eq!(idle(free(240 + 8, 16 + 784), PAGE), 512..1024);
        assert_eq!(idle(free(216, 816), PAGE), 256..1024);
    }

    #[test]
    fn pages_spanned() {
        assert_eq!(spanned(0, 0, PAGE), 0);
        assert_eq!(spanned(0, 1, PAGE), 1);
        assert_eq!(spanned(255, 257, PAGE), 2);
        assert_eq!(spanned(256, 512, PAGE), 1);
    }

    /// Overwrite the idle pages of every free block, as a kernel that took them back and
    /// returned them with other contents would.
    fn scribble(blocks: impl Iterator<Item = Block>) -> usize {
        let mut pages = 0;
        for range in idle_ranges(blocks, PAGE) {
            assert!(
                range
                    .start
                    .is_multiple_of(PAGE)
                    && range
                        .end
                        .is_multiple_of(PAGE)
            );
            // SAFETY: the range is inside a free block of a region the test owns.
            unsafe { core::ptr::write_bytes(range.start as *mut u8, 0xa5, range.len()) };
            pages += range.len() / PAGE;
        }
        pages
    }

    /// A random workload with the idle pages overwritten every few operations: if `idle`
    /// named a page the allocator still reads, `check` or a later operation finds out.
    fn churn(
        mut allocate: impl FnMut(usize) -> Option<usize>,
        mut free: impl FnMut(usize),
        mut scribble_and_check: impl FnMut() -> usize,
    ) -> usize {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        let mut held: Vec<(usize, usize, u8)> = Vec::new();
        let mut pages = 0;
        for step in 0..4000 {
            if held.len() < 24 && random.below(3) != 0 {
                let size = 1 + random.below(1500);
                if let Some(addr) = allocate(size) {
                    let fill = step as u8;
                    unsafe { core::ptr::write_bytes(addr as *mut u8, fill, size) };
                    held.push((addr, size, fill));
                }
            } else if !held.is_empty() {
                let (addr, size, fill) = held.swap_remove(random.below(held.len()));
                let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                assert!(
                    bytes
                        .iter()
                        .all(|&b| b == fill),
                    "an allocated block was overwritten"
                );
                free(addr);
            }
            if step % 7 == 0 {
                pages += scribble_and_check();
            }
        }
        pages
    }

    #[test]
    fn a_tlsf_never_reads_the_idle_pages_of_its_free_blocks() {
        let mut memory = vec![0u128; 32 * 1024 / 16];
        let tlsf = core::cell::RefCell::new(unsafe { Tlsf::new(memory.as_mut_ptr() as usize, 32 * 1024) });
        let pages = churn(
            |size| {
                tlsf.borrow_mut()
                    .allocate(size, 16)
            },
            |addr| {
                unsafe {
                    tlsf.borrow_mut()
                        .free(addr)
                }
                .unwrap()
            },
            || {
                let pages = scribble(
                    tlsf.borrow()
                        .blocks(),
                );
                tlsf.borrow()
                    .check()
                    .unwrap();
                pages
            },
        );
        assert!(pages > 1000, "the test overwrote only {pages} pages");
    }

    #[test]
    fn a_fit_never_reads_the_idle_pages_of_its_free_blocks() {
        for policy in [Policy::FirstFit, Policy::NextFit, Policy::BestFit] {
            for order in [Order::Lifo, Order::Address] {
                let mut memory = vec![0u128; 32 * 1024 / 16];
                let fit = core::cell::RefCell::new(unsafe {
                    Fit::new(memory.as_mut_ptr() as usize, 32 * 1024, policy, order)
                });
                let pages = churn(
                    |size| {
                        fit.borrow_mut()
                            .allocate(size, 16)
                    },
                    |addr| {
                        unsafe {
                            fit.borrow_mut()
                                .free(addr)
                        }
                        .unwrap()
                    },
                    || {
                        let pages = scribble(
                            fit.borrow()
                                .blocks(),
                        );
                        fit.borrow()
                            .check()
                            .unwrap();
                        pages
                    },
                );
                assert!(pages > 1000, "the test overwrote only {pages} pages");
            }
        }
    }
}
