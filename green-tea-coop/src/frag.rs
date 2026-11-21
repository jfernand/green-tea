//! The fragmentation test, `cargo xtask kernel --kernel frag`: how much memory an
//! allocator can be made to need, and how much a random workload makes it need.
//!
//! First a region that is half free and can't hold a block a quarter of its size. Then
//! green-tea-fit's adversary, a program that frees whatever leaves the most holes
//! too small to use, against each policy. Then the random workload of the `fit` test, as
//! a trace, replayed against each policy with its fragmentation measured the way
//! green-tea-shell's `replay` measures a recorded trace.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use green_tea_bare::fit::adversary::{self, Round};
use green_tea_bare::fit::trace::{self, Event, Random};
use green_tea_bare::fit::{Fit, Order, Policy};
use green_tea_bare::println;
use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::screen::Screen;

/// The region each allocator gets, from the kernel's heap.
const REGION: usize = 1024 * 1024;
/// What the adversary may hold at once, and the sizes it asks for.
const BUDGET: usize = 64 * 1024;
const SMALLEST: usize = 16;
const LARGEST: usize = 4096;

/// The `fit` test's workload: see `workload` in fit.rs.
const OPERATIONS: usize = 20_000;
const LIVE: usize = 400;

const WAYS: [(Policy, &str, Order, &str); 6] = [
    (Policy::FirstFit, "first fit", Order::Lifo, "LIFO"),
    (Policy::FirstFit, "first fit", Order::Address, "address"),
    (Policy::NextFit, "next fit", Order::Lifo, "LIFO"),
    (Policy::NextFit, "next fit", Order::Address, "address"),
    (Policy::BestFit, "best fit", Order::Lifo, "LIFO"),
    (Policy::BestFit, "best fit", Order::Address, "address"),
];

pub fn test(fb: Framebuffer) {
    let mut memory = vec![0u128; REGION / 16];
    let base = memory.as_mut_ptr() as usize;
    match holes(base).and_then(|()| adversaries(base)).and_then(|worst| Ok((worst, random_workload(base)?))) {
        Err(e) => println!("FAIL: {e}"),
        Ok((worst, (least, most))) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: an adversary holding {BUDGET} bytes made every policy need {worst}; a random workload made \
                 them need {least} to {most} over its peak"
            );
        }
    }
}

/// Fill a small region with blocks of 100 bytes, free every other one, and ask for 200.
fn holes(base: usize) -> Result<(), &'static str> {
    const SMALL: usize = 1024 + 16; // eight blocks of 128, and the two words at the ends
    // SAFETY: the region is ours, and this is the only allocator using it.
    let mut fit = unsafe { Fit::new(base, SMALL, Policy::FirstFit, Order::Address) };
    let mut blocks = Vec::new();
    while let Some(addr) = fit.allocate(100, 8) {
        blocks.push(addr);
    }
    println!("a region of {} bytes, filled with {} blocks of 100 bytes:", fit.capacity(), blocks.len());
    println!("  {}", map(&fit));
    for addr in blocks.iter().step_by(2) {
        // SAFETY: each address came from `allocate`, and is freed once.
        unsafe { fit.free(*addr) }.map_err(|_| "an allocated block couldn't be freed")?;
    }
    let free: usize = fit.free_list().map(|b| b.size).sum();
    println!("every other block freed:");
    println!("  {}", map(&fit));
    println!(
        "  {free} bytes free in {} blocks, the largest {}; {} bytes in use",
        fit.free_list().count(),
        fit.largest_free(),
        fit.used()
    );
    let refused = fit.allocate(200, 8).is_none();
    println!("  a request for 200 bytes: {}", if refused { "refused" } else { "granted" });
    let low = fit.allocate(100, 8);
    println!(
        "  a request for 100 bytes: {}; the footprint is {} bytes",
        if low == blocks.first().copied() {
            "granted, where the first block was"
        } else {
            "not where the first block was"
        },
        fit.footprint()
    );
    fit.check()?;
    if !refused || free < 2 * 216 || blocks.len() != 8 {
        return Err("a region with no hole of 216 bytes found room for 200");
    }
    if low != blocks.first().copied() || fit.footprint() != fit.capacity() {
        return Err("the footprint went down when a block was allocated below it");
    }
    Ok(())
}

/// The region's blocks in address order, by size, the free ones in parentheses.
fn map(fit: &Fit) -> String {
    let mut text = String::new();
    for block in fit.blocks() {
        let _ = if block.used { write!(text, " {} ", block.size) } else { write!(text, "({})", block.size) };
    }
    text
}

/// Run the adversary against each policy. Returns how many times its budget it made them
/// need.
fn adversaries(base: usize) -> Result<String, &'static str> {
    println!(
        "an adversary that holds at most {BUDGET} bytes, and asks for blocks of {SMALLEST} bytes, then twice that, \
         up to {LARGEST}:"
    );
    let mut first: Vec<Round> = Vec::new();
    for (policy, policy_name, order, order_name) in WAYS {
        // SAFETY: the region is ours, and this is the only allocator using it.
        let mut fit = unsafe { Fit::new(base, REGION, policy, order) };
        let rounds = adversary::run(&mut fit, BUDGET, SMALLEST, LARGEST);
        fit.check()?;
        let Some(last) = rounds.last() else {
            return Err("the adversary got nowhere");
        };
        if first.is_empty() {
            println!("  asks for  keeps  of bytes  allocates   holds  footprint  times the budget");
            for r in &rounds {
                println!(
                    "  {:8} {:6} {:9} {:10} {:7} {:10} {:>17}",
                    r.size,
                    r.kept,
                    r.kept_bytes,
                    r.allocated,
                    r.live,
                    r.footprint,
                    times(r.footprint, BUDGET)
                );
            }
            println!("the footprint at the end, policy by policy:");
        }
        println!("  {policy_name:10} {order_name:8} {:10} {:>8}", last.footprint, times(last.footprint, BUDGET));
        if rounds.len() != (LARGEST / SMALLEST).ilog2() as usize + 1 {
            return Err("the adversary ran out of region");
        }
        if rounds.iter().any(|r| r.live > BUDGET) {
            return Err("the adversary held more than its budget");
        }
        // Every round after the first must have gone in memory never used before.
        for pair in rounds.windows(2) {
            if pair[1].footprint - pair[0].footprint != pair[1].allocated * adversary::block_size(pair[1].size) {
                return Err("an allocation went in a hole the adversary left");
            }
        }
        if first.is_empty() {
            first = rounds;
        } else if rounds != first {
            return Err("the adversary did better against one policy than another");
        }
    }
    let last = first.last().ok_or("no rounds")?;
    if last.footprint < 4 * BUDGET {
        return Err("the adversary made the allocators need less than four times its budget");
    }
    Ok(format!("{} bytes, {} times as much", last.footprint, times(last.footprint, BUDGET)))
}

/// The `fit` test's workload as a trace: the same generator, the same seed, the same
/// decisions. An address here is only a name for a block.
pub fn random_trace() -> Vec<Event> {
    let mut random = Random(0x2545_f491_4f6c_dd1d);
    let mut events = Vec::with_capacity(OPERATIONS);
    let mut live: Vec<usize> = Vec::with_capacity(LIVE + 1);
    for step in 0..OPERATIONS {
        let allocate = live.is_empty() || random.below(if live.len() < LIVE { 3 } else { 2 }) != 0;
        if allocate && live.len() <= LIVE {
            let size = if random.below(8) == 0 { 128 + random.below(1920) } else { 16 + random.below(112) };
            let name = (step + 1) * 16;
            events.push(Event::allocate(name, size, 8));
            live.push(name);
        } else {
            events.push(Event::free(live.swap_remove(random.below(live.len()))));
        }
    }
    events
}

/// Replay the random workload against each policy. Returns the least and the most that
/// the footprint exceeded the peak of live bytes.
fn random_workload(base: usize) -> Result<(String, String), &'static str> {
    let events = random_trace();
    println!(
        "{OPERATIONS} allocations and frees at random, up to {LIVE} blocks at once, in a region of {} KiB:",
        REGION / 1024
    );
    println!("  policy     order     peak live  peak used  footprint  over live then  over peak live  over peak used");
    let mut excess: Vec<(usize, usize)> = Vec::new();
    for (policy, policy_name, order, order_name) in WAYS {
        // SAFETY: as above.
        let mut fit = unsafe { Fit::new(base, REGION, policy, order) };
        let r = trace::replay(&events, &mut fit);
        fit.check()?;
        println!(
            "  {policy_name:10} {order_name:8} {:10} {:10} {:10} {:>15} {:>15} {:>15}",
            r.peak_live,
            r.peak_used,
            r.footprint,
            percent(r.footprint - r.live_at_peak_footprint, r.live_at_peak_footprint),
            percent(r.footprint - r.peak_live, r.peak_live),
            percent(r.footprint - r.peak_used, r.peak_used),
        );
        if r.failed != 0 || r.unmatched != 0 || r.allocations + r.frees != OPERATIONS {
            return Err("the workload didn't fit in its region");
        }
        if r.peak_live >= r.peak_used || r.peak_used > r.footprint {
            return Err("a replay used less memory than the workload asked for");
        }
        excess.push((r.footprint - r.peak_live, r.peak_live));
    }
    let least = excess.iter().min().ok_or("no replays")?;
    let most = excess.iter().max().ok_or("no replays")?;
    Ok((percent(least.0, least.1), percent(most.0, most.1)))
}

/// `part` as a percentage of `whole`, to one decimal place, without floating point.
fn percent(part: usize, whole: usize) -> String {
    let tenths = (part * 1000 + whole / 2) / whole.max(1);
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// `count` as a multiple of `of`, to two decimal places.
fn times(count: usize, of: usize) -> String {
    let hundredths = (count * 100 + of / 2) / of.max(1);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}
