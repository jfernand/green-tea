//! The test of the allocators that don't search, `cargo xtask kernel --kernel tlsf`.
//!
//! green-tea-fit's buddy system and its TLSF, beside the first fit and best fit of the
//! `fit` test: what block each makes for a request, what each needs for the same random
//! workload, and what each does on a heap built to make a search as long as it can be.
//! Last, the kernel's own heap, which is the TLSF.

use crate::frag::random_trace;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use green_tea_bare::fit::buddy::Buddy;
use green_tea_bare::fit::tlsf::{self, Tlsf};
use green_tea_bare::fit::trace;
use green_tea_bare::fit::{Block, Fit, Heap, Order, Policy, adversary};
use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::{Board, Platform, heap, println};

/// The region each allocator gets, from the kernel's heap. The buddy system uses the
/// largest power of two in it, which is all but its last 32 bytes.
const REGION: usize = 2 * 1024 * 1024 + 32;

/// The four allocators compared.
const NAMES: [&str; 4] = ["first fit, by address", "best fit, by address", "buddy system", "TLSF"];

/// Make allocator `which` of `NAMES` over the region at `base`, and run `f` on it.
fn with_heap<R>(which: usize, base: usize, len: usize, f: impl FnOnce(&mut dyn Heap) -> R) -> R {
    // SAFETY: the region is the test's, and one allocator uses it at a time.
    unsafe {
        match which {
            0 => f(&mut Fit::new(base, len, Policy::FirstFit, Order::Address)),
            1 => f(&mut Fit::new(base, len, Policy::BestFit, Order::Address)),
            2 => f(&mut Buddy::new(base, len)),
            _ => f(&mut Tlsf::new(base, len)),
        }
    }
}

pub fn test(fb: Framebuffer) {
    let mut memory = vec![0u128; REGION / 16];
    let base = memory.as_mut_ptr() as usize;
    let verdict = buddies(base)
        .and_then(|()| blocks())
        .and_then(|()| lists(base))
        .and_then(|()| workload(base))
        .and_then(|()| long_searches(base))
        .and_then(|longest| Ok((longest, kernel_heap()?)));
    match verdict {
        Err(e) => println!("FAIL: {e}"),
        Ok((longest, allocations)) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: with 8000 holes first fit looked at {longest} blocks for one allocation, and TLSF at 1; the \
                 kernel's heap never looked at more than 1 in {allocations} allocations"
            );
        }
    }
}

/// The blocks of a region in address order, by size, the free ones in parentheses.
fn map(blocks: impl Iterator<Item = Block>) -> String {
    let mut text = String::new();
    for block in blocks {
        let _ = if block.used { write!(text, " {} ", block.size) } else { write!(text, "({})", block.size) };
    }
    text
}

/// A buddy system over 1,024 bytes: halving, and merging with the buddy only.
fn buddies(base: usize) -> Result<(), &'static str> {
    // SAFETY: the region is the test's.
    let mut buddy = unsafe { Buddy::new(base, 1024 + 16) };
    let first = buddy.first();
    println!("a buddy system over {} bytes:", buddy.capacity());
    println!("  {}", map(buddy.blocks()));
    let mut held = Vec::new();
    for name in ["a", "b", "c", "d"] {
        let addr = buddy.allocate(100, 8).ok_or("the buddy system refused 100 bytes")?;
        held.push(addr);
        println!("{name} = allocate(100), at offset {}:", addr - 8 - first);
        println!("  {}", map(buddy.blocks()));
    }
    let offset = |addr: usize| addr - 8 - first;
    for (name, addr) in [("b", held[1]), ("c", held[2])] {
        // SAFETY: each address came from `allocate`, and is freed once.
        unsafe { buddy.free(addr) }.map_err(|_| "an allocated block couldn't be freed")?;
        println!("free({name}): its buddy is at offset {} ^ 128 = {}", offset(addr), offset(addr) ^ 128);
        println!("  {}", map(buddy.blocks()));
    }
    let large = buddy.allocate(200, 8);
    println!(
        "allocate(200), which needs a block of 256: {}",
        large.map_or(String::from("refused"), |addr| format!("at offset {}", offset(addr)))
    );
    if large.map(offset) != Some(512) || buddy.blocks().filter(|b| !b.used).count() != 3 {
        return Err("two free neighbours that aren't buddies were merged");
    }
    for (name, addr) in [("a", held[0]), ("d", held[3])] {
        unsafe { buddy.free(addr) }.map_err(|_| "an allocated block couldn't be freed")?;
        println!("free({name}):");
        println!("  {}", map(buddy.blocks()));
    }
    buddy.check()?;
    if buddy.blocks().count() != 3 || buddy.stats().merges != 3 {
        return Err("buddies that were both free weren't merged");
    }
    Ok(())
}

/// The block each allocator makes for a request, and the TLSF list a free block of that
/// size is on.
fn blocks() -> Result<(), &'static str> {
    println!("the block each allocator makes for a request:");
    println!("  request  Fit, TLSF  buddy   a free block of that size is on the TLSF list for");
    for request in [24, 100, 120, 121, 300, 500, 1000, 5000, 100_000] {
        let fit = adversary::block_size(request);
        let buddy = 32 << Buddy::order_for(request).ok_or("a request too large for an order")?;
        let (row, column) = tlsf::class(fit);
        let (from, width) = tlsf::range(row, column);
        let list = if width == 16 { format!("{from}") } else { format!("{from} to {}", from + width - 16) };
        println!("  {request:7} {fit:10} {buddy:6}   {list}  (row {row}, column {column})");
        if fit < from || fit >= from + width || buddy < request + 8 || buddy >= 2 * (request + 8).max(17) {
            return Err("a block isn't the size it should be for its request");
        }
    }
    Ok(())
}

/// A TLSF over 4 KiB: which lists hold a block, as blocks are allocated and freed.
fn lists(base: usize) -> Result<(), &'static str> {
    // SAFETY: the region is the test's.
    let mut tlsf = unsafe { Tlsf::new(base, 4096) };
    let show = |tlsf: &Tlsf, what: &str| {
        let mut text = String::new();
        for (row, column, count) in tlsf.lists() {
            let (from, width) = tlsf::range(row, column);
            let _ = write!(text, " {count} on the list for {from}");
            if width > 16 {
                let _ = write!(text, " to {}", from + width - 16);
            }
            let _ = write!(text, ";");
        }
        println!("{what}");
        println!("  {}", map(tlsf.blocks()));
        println!("  free:{}", text.trim_end_matches(';'));
    };
    show(&tlsf, "a TLSF over 4,096 bytes:");
    let _a = tlsf.allocate(100, 8).ok_or("TLSF refused 100 bytes")?;
    let b = tlsf.allocate(500, 8).ok_or("TLSF refused 500 bytes")?;
    let _c = tlsf.allocate(24, 8).ok_or("TLSF refused 24 bytes")?;
    show(&tlsf, "a = allocate(100), b = allocate(500), c = allocate(24):");
    // SAFETY: `b` came from `allocate`, and is freed once.
    unsafe { tlsf.free(b) }.map_err(|_| "an allocated block couldn't be freed")?;
    show(&tlsf, "free(b):");
    let again = tlsf.allocate(500, 8);
    show(&tlsf, "allocate(500) again, which needs a block of 528:");
    if again == Some(b) {
        return Err("TLSF took a block from a list that could have held one too small");
    }
    let smaller = tlsf.allocate(480, 8);
    show(&tlsf, "allocate(480), which needs a block of 496:");
    if smaller != Some(b) {
        return Err("TLSF passed over a block on a list whose every block fits");
    }
    tlsf.check()?;
    Ok(())
}

/// The `fit` test's random workload, replayed against each of the four.
fn workload(base: usize) -> Result<(), &'static str> {
    let events = random_trace();
    println!("{} allocations and frees at random, as in the fit and frag tests:", events.len());
    println!(
        "  allocator               peak live  peak used  footprint  looked at: mean   most  walked past: mean   most"
    );
    for (which, name) in NAMES.into_iter().enumerate() {
        let r = with_heap(which, base, REGION, |heap| {
            let report = trace::replay(&events, heap);
            heap.check().map(|()| report)
        })?;
        let s = r.stats;
        println!(
            "  {name:22} {:10} {:10} {:10} {:>15} {:6} {:>17} {:6}",
            r.peak_live,
            r.peak_used,
            r.footprint,
            ratio(s.examined, s.allocations),
            s.longest_search,
            ratio(s.passed, s.frees),
            s.longest_walk
        );
        if r.failed != 0 || r.unmatched != 0 || r.left == 0 {
            return Err("the workload didn't fit in its region");
        }
        if which == 3 && (s.longest_search != 1 || s.longest_walk != 0) {
            return Err("TLSF searched for a block");
        }
    }
    Ok(())
}

/// Fill the region with small blocks, free every other one, and then allocate and free a
/// block larger than any hole, a hundred times: the allocation that has to pass every
/// hole, and the free that has to find its place among them. Returns how many blocks
/// first fit looked at for one such allocation with the most holes.
fn long_searches(base: usize) -> Result<usize, &'static str> {
    const PAIRS: usize = 100;
    println!(
        "an allocation of 4,096 bytes and its free, with holes of 64 bytes in the way ({PAIRS} of each, averaged):"
    );
    println!("  holes  allocator               looked at  walked past  microseconds");
    let mut longest = 0;
    for holes in [1000, 2000, 4000, 8000] {
        for (which, name) in NAMES.into_iter().enumerate() {
            let (examined, passed, micros) = with_heap(which, base, REGION, |heap| {
                let mut small = Vec::with_capacity(2 * holes);
                for _ in 0..2 * holes {
                    small.push(heap.allocate(48, 8).ok_or("the region is too small for the holes")?);
                }
                for addr in small.iter().step_by(2) {
                    // SAFETY: each address came from `allocate`, and is freed once.
                    unsafe { heap.free(*addr) }.map_err(|_| "an allocated block couldn't be freed")?;
                }
                let before = heap.stats();
                let start = Board::ticks();
                for _ in 0..PAIRS {
                    let addr = heap.allocate(4096, 8).ok_or("the region is too small for the large block")?;
                    unsafe { heap.free(addr) }.map_err(|_| "an allocated block couldn't be freed")?;
                }
                let ticks = Board::ticks() - start;
                let after = heap.stats();
                heap.check()?;
                let micros_x10 = ticks * 10_000 / Board::ticks_per_ms() / PAIRS as u64;
                Ok::<_, &'static str>((
                    (after.examined - before.examined) / PAIRS,
                    (after.passed - before.passed) / PAIRS,
                    micros_x10,
                ))
            })?;
            println!("  {holes:5}  {name:22} {examined:10} {passed:12} {:>10}.{}", micros / 10, micros % 10);
            match which {
                0 if examined != holes + 1 || passed != holes => return Err("first fit didn't pass every hole"),
                0 => longest = examined,
                3 if examined != 1 || passed != 0 => return Err("TLSF searched for a block"),
                _ => {}
            }
        }
    }
    Ok(longest)
}

/// The kernel's heap: which allocator it is, and the longest search it has made since
/// boot. Returns how many allocations that was over.
fn kernel_heap() -> Result<usize, &'static str> {
    let stats = heap::with(|tlsf| tlsf.stats());
    println!(
        "the kernel's heap ({}): {} allocations and {} frees since boot; the most blocks one allocation looked at: \
         {}; the most one free walked past: {}",
        heap::NAME,
        stats.allocations,
        stats.frees,
        stats.longest_search,
        stats.longest_walk
    );
    heap::with(|tlsf| tlsf.check())?;
    if stats.longest_search != 1 || stats.longest_walk != 0 || stats.allocations == 0 {
        return Err("the kernel's heap searched for a block");
    }
    Ok(stats.allocations)
}

/// A count divided by another, to one decimal place, without floating point.
fn ratio(count: usize, per: usize) -> String {
    let tenths = (count * 10 + per / 2) / per.max(1);
    format!("{}.{}", tenths / 10, tenths % 10)
}
