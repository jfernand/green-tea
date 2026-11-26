//! The heap's pages, mapped on demand.
//!
//! linker.ld reserves 4 MiB for the heap: 1,024 pages of 4 KiB. On a board with
//! [`Platform::DEMAND_PAGING`] (riscv64 and x86_64) the page tables start out with every
//! one of them missing. The first access to a page faults; the board's fault handler calls
//! [`fault`], which maps the page and returns, and the access is made again. A page of the
//! heap that nothing touches is never mapped.
//!
//! The map is still the identity: the page at an address gets the frame at that address.
//! The drivers rely on it, since they hand heap addresses to devices, which know nothing
//! of page tables. So no frame is saved for anyone else yet, and what this module keeps is
//! the count: how many pages the heap has needed, against how many it reserved.
//!
//! [`release`] takes pages back. It zeroes each one and unmaps it, so that a released page
//! reads as zeros when it is next touched, as a page given back to Linux with
//! `madvise(MADV_DONTNEED)` does. [`trim`] releases the pages of the heap's free blocks
//! that hold none of the allocator's own words.
//!
//! On aarch64 the MMU is off: every page is always there, nothing faults, and `release`
//! does nothing.

use crate::heap;
use crate::platform::{Board, Platform, TimerInterrupt};
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use green_tea_fit::pages::idle_ranges;
use green_tea_memory::PhysicalRange;

pub const PAGE: usize = 4096;

unsafe extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

// Atomics, since any CPU can take a fault.
static FAULTS: AtomicUsize = AtomicUsize::new(0);
static MAPPED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static RELEASED: AtomicUsize = AtomicUsize::new(0);
/// Set by `map_all`: the whole heap is mapped, and stays mapped.
static WHOLE: AtomicBool = AtomicBool::new(false);

/// Count one more page mapped.
fn count_mapped() {
    let mapped = MAPPED.fetch_add(1, Relaxed) + 1;
    PEAK.fetch_max(mapped, Relaxed);
}

/// What has happened to the heap's pages so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stats {
    /// The pages linker.ld reserves.
    pub reserved: usize,
    /// The pages mapped now, and the most there have been at once.
    pub mapped: usize,
    pub peak: usize,
    /// The page faults that mapped a page.
    pub faults: usize,
    /// The pages [`release`] unmapped.
    pub released: usize,
}

/// One step of a page-table walk: the entry a table has for an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Step {
    /// Which of the table's 512 entries the address selects, and what it holds.
    pub index: usize,
    pub entry: u64,
    pub points_to: PointsTo,
}

/// What a page-table entry says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PointsTo {
    /// Nothing: an access to the address faults.
    Nothing,
    /// The table for the next level, at this address.
    Table(usize),
    /// A page of `size` bytes whose frame starts at `frame`: the walk ends here.
    Page { frame: usize, size: usize },
}

/// Walk the page table for `addr` as the CPU would, from the root: one step per level,
/// ending at the first entry that isn't a pointer to another table. Nothing on a board
/// that doesn't page.
pub fn walk(addr: usize) -> impl Iterator<Item = Step> {
    Board::walk(addr)
        .into_iter()
        .flatten()
}

/// The heap's addresses. Both ends are multiples of [`PAGE`].
pub fn heap_range() -> Range<usize> {
    heap_physical_range().as_range()
}

/// The checked physical range reserved for the heap by linker.ld.
pub fn heap_physical_range() -> PhysicalRange {
    let start = core::hint::black_box(&raw const __heap_start as usize);
    let end = core::hint::black_box(&raw const __heap_end as usize);
    PhysicalRange::from_bounds(start, end, PAGE).expect("linker heap range must be page-aligned and nonempty")
}

pub fn stats() -> Stats {
    let reserved = heap_range().len() / PAGE;
    if !Board::DEMAND_PAGING {
        return Stats { reserved, mapped: reserved, peak: reserved, faults: 0, released: 0 };
    }
    Stats {
        reserved,
        mapped: MAPPED.load(Relaxed),
        peak: PEAK.load(Relaxed),
        faults: FAULTS.load(Relaxed),
        released: RELEASED.load(Relaxed),
    }
}

/// Called by a board's fault handler for a page fault at `addr`, on the fault stack, with
/// interrupts off. If the address is in a page of the heap that isn't mapped, map the page
/// and return true: the handler then returns to the faulting instruction, which runs
/// again. Anything else is not ours, and the handler panics.
///
/// It must not touch the heap, and doesn't: a fault in here would overwrite the frame of
/// the fault being handled.
pub fn fault(addr: usize) -> bool {
    if !Board::DEMAND_PAGING || !heap_range().contains(&addr) {
        return false;
    }
    let was_mapped = Board::set_mapped(addr & !(PAGE - 1), true);
    if was_mapped {
        return false; // a fault on a page that is there: not for want of a mapping
    }
    FAULTS.fetch_add(1, Relaxed);
    count_mapped();
    true
}

/// Map every page of the heap that isn't mapped yet, and release none from now on.
///
/// Demand paging here is for one CPU: unmapping a page flushes one CPU's TLB, and nothing
/// tells the others. So before a kernel starts its other CPUs ([`crate::cpus::start_all`]) the heap is mapped whole, and after that
/// no CPU faults on it and no page table entry changes.
pub fn map_all() {
    if !Board::DEMAND_PAGING {
        return;
    }
    WHOLE.store(true, Relaxed);
    for page in heap_range().step_by(PAGE) {
        Board::without_interrupts(|| {
            if !Board::set_mapped(page, true) {
                count_mapped();
            }
        });
    }
}

/// Whether the page of the heap that holds `addr` is mapped.
pub fn is_mapped(addr: usize) -> bool {
    !Board::DEMAND_PAGING || Board::is_mapped(addr & !(PAGE - 1))
}

/// How many pages that hold any of `range`'s bytes are mapped. The range must be in the heap.
pub fn mapped_in(range: Range<usize>) -> usize {
    assert!(heap_range().start <= range.start && range.end <= heap_range().end);
    let first = range.start & !(PAGE - 1);
    (first..range.end)
        .step_by(PAGE)
        .filter(|page| is_mapped(*page))
        .count()
}

/// Call `f` with each run of mapped pages, lowest first, as page numbers counted from the
/// start of the heap: `f(0..3)` for the first three.
pub fn for_each_run(mut f: impl FnMut(Range<usize>)) {
    let heap = heap_range();
    let pages = heap.len() / PAGE;
    let mut start = None;
    for page in 0..=pages {
        let mapped = page < pages && is_mapped(heap.start + page * PAGE);
        match (mapped, start) {
            (true, None) => start = Some(page),
            (false, Some(first)) => {
                f(first..page);
                start = None;
            }
            _ => {}
        }
    }
}

/// Give back every whole page inside `range`: zero it, unmap it, and forget its
/// translation. Returns how many pages that was. The next access to any of them faults,
/// and finds zeros.
///
/// # Safety
/// Nothing in those pages may be needed again: `range` must be memory that is free, in the
/// heap, and not about to be read.
pub unsafe fn release(range: Range<usize>) -> usize {
    if !Board::DEMAND_PAGING || WHOLE.load(Relaxed) {
        return 0;
    }
    let heap = heap_range();
    let start = range
        .start
        .max(heap.start)
        .next_multiple_of(PAGE);
    let end = range
        .end
        .min(heap.end)
        & !(PAGE - 1);
    let mut released = 0;
    for page in (start..end).step_by(PAGE) {
        // A timer interrupt between the test and the unmapping could touch the page.
        Board::without_interrupts(|| {
            if Board::is_mapped(page) {
                // SAFETY: the page is mapped, and the caller says nothing needs it.
                unsafe { core::ptr::write_bytes(page as *mut u8, 0, PAGE) };
                Board::set_mapped(page, false);
                MAPPED.fetch_sub(1, Relaxed);
                RELEASED.fetch_add(1, Relaxed);
                released += 1;
            }
        });
    }
    released
}

/// Release the pages of the heap's free blocks that hold neither a header, nor a link,
/// nor a footer. Returns how many pages that was.
pub fn trim() -> usize {
    heap::with(|heap| {
        let mut released = 0;
        for range in idle_ranges(heap.blocks(), PAGE) {
            // SAFETY: the allocator reads nothing in a free block but those four words.
            released += unsafe { release(range) };
        }
        released
    })
}
