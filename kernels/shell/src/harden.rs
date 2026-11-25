//! `harden`: the recorded trace replayed against green-tea-fit's hardened allocator, with
//! one defence more each time, to see what each costs a program in memory and in searching.
//!
//! `Hardened` with no defence is `Fit` with first fit over a LIFO list, so its row is the
//! first row of `replay`'s table.

use crate::Shell;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use green_tea_bare::fit::hardened::{ALL, CANARY, ENCODE, Hardened, QUARANTINE, UNLINK};
use green_tea_bare::fit::trace::{self, Event};
use green_tea_bare::println;

/// The region each replay gets, from the heap: as `replay`'s.
const REGION: usize = 512 * 1024;
const SEED: u64 = 0x2545_f491_4f6c_dd1d;

const L1: u8 = UNLINK;
const L2: u8 = L1 | ENCODE;
const L3: u8 = L2 | CANARY;
const L4: u8 = L3 | QUARANTINE;

/// What one replay needed.
struct Cost {
    peak_live: usize,
    peak_used: usize,
    peak_held: usize,
    footprint: usize,
}

impl Shell {
    /// `harden`: replay the recorded trace against each hardened allocator, print what
    /// each needed, and judge the result.
    pub fn harden(&mut self) {
        let Some(recording) = &self.trace else {
            return println!("nothing is recorded: `trace on`, some commands, `trace off`");
        };
        if recording.dropped != 0 {
            return println!("FAIL: the trace is incomplete: events were dropped");
        }
        match report(&recording.events[..recording.len]) {
            Ok(summary) => println!("PASS: {summary}"),
            Err(e) => println!("FAIL: {e}"),
        }
    }
}

fn replayed<const D: u8>(base: usize, events: &[Event], slots: usize, name: &str) -> Result<Cost, &'static str> {
    // SAFETY: the region is the caller's, and one allocator uses it at a time.
    let mut heap = unsafe { Hardened::<D>::new(base, REGION, SEED) };
    heap.set_quarantine(slots);
    let r = trace::replay(events, &mut heap);
    heap.check()?;
    let (stats, counts) = (heap.stats(), heap.counts());
    println!(
        "  {name:17} {:10} {:10} {:10} {:10} {:>10} {:>12}",
        r.peak_live,
        r.peak_used,
        counts.peak_held,
        r.footprint,
        ratio(stats.examined, stats.allocations),
        ratio(counts.poisoned + counts.verified, stats.frees),
    );
    if r.failed != 0 {
        return Err("the trace didn't fit in the region it was replayed in");
    }
    Ok(Cost { peak_live: r.peak_live, peak_used: r.peak_used, peak_held: counts.peak_held, footprint: r.footprint })
}

fn report(events: &[Event]) -> Result<String, &'static str> {
    let mut memory = vec![0u128; REGION / 16];
    let base = memory.as_mut_ptr() as usize;
    println!("the trace replayed against the hardened allocator, each time in a region of {} KiB:", REGION / 1024);
    println!("  allocator          peak live  peak used  peak held  footprint  looked at  words filled");
    println!("                                                                per alloc  and checked");
    println!("                                                                           per free");
    let rows = [
        replayed::<0>(base, events, 16, "no defence")?,
        replayed::<L1>(base, events, 16, "+ safe unlinking")?,
        replayed::<L2>(base, events, 16, "+ encoded links")?,
        replayed::<L3>(base, events, 16, "+ canary")?,
        replayed::<L4>(base, events, 16, "+ quarantine")?,
        replayed::<ALL>(base, events, 16, "+ random")?,
    ];
    if rows.iter().any(|row| row.peak_live != rows[0].peak_live) {
        return Err("two replays of one trace disagree about what the program asked for");
    }
    if (rows[1].footprint, rows[2].footprint) != (rows[0].footprint, rows[0].footprint) {
        return Err("checking and encoding links shouldn't change where blocks go");
    }
    if rows[3].peak_used <= rows[2].peak_used || rows[3].peak_held != 0 || rows[4].peak_held == 0 {
        return Err("a canary should cost bytes in blocks, and only a quarantine should hold any");
    }
    println!("with a quarantine of 0, 4, 16 and 64 blocks, and no random choice:");
    let mut held = 0;
    for slots in [0, 4, 16, 64] {
        let cost = replayed::<L4>(base, events, slots, &format!("{slots} blocks"))?;
        if cost.peak_held < held {
            return Err("a longer quarantine should hold more");
        }
        held = cost.peak_held;
    }
    Ok(format!(
        "a peak of {} live bytes needed {} with no defence, {} with a canary, {} with a quarantine of 16 blocks and \
         {} with random choices",
        rows[0].peak_live, rows[0].footprint, rows[3].footprint, rows[4].footprint, rows[5].footprint
    ))
}

/// A count divided by another, to one decimal place, without floating point.
fn ratio(count: usize, per: usize) -> String {
    let tenths = (count * 10 + per / 2) / per.max(1);
    format!("{}.{}", tenths / 10, tenths % 10)
}
