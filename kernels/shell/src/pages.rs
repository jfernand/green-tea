//! `pages`: how many of the heap's pages are mapped, and what a recorded trace needs when
//! its footprint is counted in pages.
//!
//! `pages` prints the heap's reserved, mapped and released pages. `pages trim` gives back
//! the pages of the heap's free blocks that hold nothing. `pages replay` replays the
//! recorded trace against each allocator in a region none of whose pages are mapped, with
//! every block written to as the shell wrote to it, and counts the pages that got mapped:
//! the trace's footprint as the page table saw it. Then it trims each allocator's free
//! blocks, and counts again.

use crate::Shell;
use alloc::alloc::{alloc, dealloc};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::alloc::Layout;
use green_tea_bare::fit::pages::{idle_ranges, spanned};
use green_tea_bare::fit::tlsf::Tlsf;
use green_tea_bare::fit::trace::{self, Event};
use green_tea_bare::fit::{Block, Fit, FreeError, Heap, Order, Policy, Stats};
use green_tea_bare::pages::{self, PAGE};
use green_tea_bare::{Board, Platform, heap, println};

/// The region each replay gets: 64 pages, from the heap.
const REGION: usize = 256 * 1024;
/// What the replay writes into every block it allocates.
const FILL: u8 = 0x5a;

const WAYS: [(Policy, Order, &str); 6] = [
    (Policy::FirstFit, Order::Lifo, "first fit  LIFO"),
    (Policy::FirstFit, Order::Address, "first fit  address"),
    (Policy::NextFit, Order::Lifo, "next fit   LIFO"),
    (Policy::NextFit, Order::Address, "next fit   address"),
    (Policy::BestFit, Order::Lifo, "best fit   LIFO"),
    (Policy::BestFit, Order::Address, "best fit   address"),
];

/// An allocator whose every block is written to in full when it is allocated, as a
/// program writes to what it asks for. Replaying a trace against the allocator alone would
/// touch only the pages its tags and links are on.
struct Written<'a, H>(&'a mut H);

impl<H: Heap> Heap for Written<'_, H> {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        let addr = self
            .0
            .allocate(size, align)?;
        // SAFETY: the allocator handed out `size` bytes at `addr`.
        unsafe { core::ptr::write_bytes(addr as *mut u8, FILL, size) };
        Some(addr)
    }
    unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError> {
        unsafe {
            self.0
                .free(addr)
        }
    }
    fn used(&self) -> usize {
        self.0
            .used()
    }
    fn footprint(&self) -> usize {
        self.0
            .footprint()
    }
    fn stats(&self) -> Stats {
        self.0
            .stats()
    }
    fn check(&self) -> Result<(), &'static str> {
        self.0
            .check()
    }
}

/// What one allocator made of the trace, in pages.
#[derive(Clone, Copy)]
struct Row {
    /// The footprint in bytes, and the pages from the start of the region to its end.
    footprint: usize,
    footprint_pages: usize,
    /// The pages mapped when the replay ended, and after the free blocks were trimmed.
    touched: usize,
    trimmed: usize,
}

impl Shell {
    /// `pages`, `pages trim`, `pages replay`.
    pub fn pages(&mut self, arg: &str) {
        if !Board::DEMAND_PAGING {
            let reserved = pages::stats().reserved;
            return println!(
                "{} runs with the MMU off: all {reserved} pages of the heap are there, always",
                Board::NAME
            );
        }
        match arg {
            "" => status(),
            "trim" => {
                let released = pages::trim();
                println!("released {released} pages");
                status();
            }
            "replay" => match &self.trace {
                None => println!("nothing is recorded: `trace on`, some commands, `trace off`"),
                Some(recording) if recording.dropped != 0 => println!("FAIL: the trace is incomplete"),
                Some(recording) => match replay(&recording.events[..recording.len]) {
                    Ok((least, most)) => println!(
                        "PASS: the recorded trace touched {least} pages under the allocator that needed fewest, and \
                         {most} under the one that needed most"
                    ),
                    Err(e) => println!("FAIL: {e}"),
                },
            },
            _ => println!("usage: pages [trim|replay]"),
        }
    }
}

/// One line: the heap's pages.
fn status() {
    let stats = pages::stats();
    let footprint = heap::with(|heap| heap.footprint());
    println!(
        "heap: {} pages reserved; a footprint of {} pages; {} mapped, {} at most; {} page faults, {} pages released",
        stats.reserved,
        footprint.div_ceil(PAGE),
        stats.mapped,
        stats.peak,
        stats.faults,
        stats.released
    );
}

/// The bytes the trace's program still held when the trace ended.
fn live_at_the_end(events: &[Event]) -> (usize, usize) {
    let mut blocks = BTreeMap::new();
    for event in events {
        if event.is_free() {
            blocks.remove(&event.addr);
        } else {
            blocks.insert(event.addr, event.size as usize);
        }
    }
    (
        blocks.len(),
        blocks
            .values()
            .sum(),
    )
}

fn replay(events: &[Event]) -> Result<(usize, usize), &'static str> {
    let layout = Layout::from_size_align(REGION, PAGE).expect("a layout of whole pages");
    // SAFETY: the layout has a size.
    let base = unsafe { alloc(layout) } as usize;
    if base == 0 {
        return Err("no room in the heap for a region to replay in");
    }
    let (blocks, bytes) = live_at_the_end(events);
    println!(
        "the recorded trace, replayed in a region of {} pages that starts with none mapped, every block written to:",
        REGION / PAGE
    );
    println!("  allocator            footprint  in pages  pages touched  after trimming");
    let mut rows = Vec::with_capacity(WAYS.len() + 1);
    let mut result = Ok(());
    for (policy, order, name) in WAYS {
        // SAFETY: the region is ours, and one allocator uses it at a time.
        let row = measure(
            events,
            base,
            |region| unsafe { Fit::new(region, REGION, policy, order) },
            |fit| {
                fit.blocks()
                    .collect()
            },
        );
        result = result.and(row.map(|row| print(name, &row, &mut rows)));
    }
    let row = measure(
        events,
        base,
        |region| unsafe { Tlsf::new(region, REGION) },
        |tlsf| {
            tlsf.blocks()
                .collect()
        },
    );
    result = result.and(row.map(|row| print("TLSF", &row, &mut rows)));
    println!(
        "  at the end {blocks} blocks are live, of {bytes} bytes in all: {} pages, if they were side by side",
        bytes.div_ceil(PAGE)
    );
    // SAFETY: allocated above with this layout. Its pages go back with it.
    unsafe {
        pages::release(base..base + REGION);
        dealloc(base as *mut u8, layout);
    }
    result?;
    for row in &rows {
        // The allocator's marker at the end of the region is on a page of its own, so a
        // replay touches one page more than its footprint spans, and never fewer pages
        // than the footprint spans.
        if row.touched < row.footprint_pages || row.touched > row.footprint_pages + 1 {
            return Err("a replay touched pages beyond its footprint, or not all of those within it");
        }
        if row.trimmed > row.touched || row.trimmed < bytes.div_ceil(PAGE) {
            return Err("trimming left more pages than it found, or fewer than the live blocks need");
        }
    }
    let touched = rows
        .iter()
        .map(|row| row.touched);
    Ok((
        touched
            .clone()
            .min()
            .unwrap_or(0),
        touched
            .max()
            .unwrap_or(0),
    ))
}

fn print(name: &str, row: &Row, rows: &mut Vec<Row>) {
    println!("  {name:19} {:10} {:9} {:14} {:15}", row.footprint, row.footprint_pages, row.touched, row.trimmed);
    rows.push(*row);
}

/// Unmap the region, make an allocator in it, replay the trace against it with every
/// block written to, and count the region's mapped pages: then trim the allocator's free
/// blocks and count again.
fn measure<H: Heap>(
    events: &[Event],
    base: usize,
    new: impl FnOnce(usize) -> H,
    blocks: impl Fn(&H) -> Vec<Block>,
) -> Result<Row, &'static str> {
    let region = base..base + REGION;
    // SAFETY: the region is ours, and the allocator that used it last is gone.
    unsafe { pages::release(region.clone()) };
    if pages::mapped_in(region.clone()) != 0 {
        return Err("a released region still has pages mapped");
    }
    let mut heap = new(base);
    let report = trace::replay(events, &mut Written(&mut heap));
    if report.failed != 0 {
        return Err("the trace didn't fit in the region it was replayed in");
    }
    let touched = pages::mapped_in(region.clone());
    let all = blocks(&heap);
    let first = all
        .first()
        .map_or(base, |block| block.start);
    for range in idle_ranges(
        all.iter()
            .copied(),
        PAGE,
    ) {
        // SAFETY: inside a free block of an allocator that reads none of it.
        unsafe { pages::release(range) };
    }
    heap.check()?;
    // Every block still allocated was written to when it was allocated: its first byte
    // must have survived the trimming.
    if all
        .iter()
        .any(|block| block.used && unsafe { ((block.start + 8) as *const u8).read_volatile() } != FILL)
    {
        return Err("trimming took a page that an allocated block was on");
    }
    Ok(Row {
        footprint: report.footprint,
        footprint_pages: spanned(base, first + report.footprint, PAGE),
        touched,
        trimmed: pages::mapped_in(region),
    })
}
