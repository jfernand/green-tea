//! `session` and `replay`: a script for the shell to run while the heap records, and the
//! replay of what was recorded against each of green-tea-fit's policies.
//!
//! The replay answers one question for each policy: how large a region would this program
//! have needed? It prints the answer three times. Once for the trace as recorded. Once for
//! a trace with the same sizes and the same lifetimes dealt out at random, which is how
//! allocators were tested for decades. And once with the long-lived blocks given a region
//! of their own.

use crate::{Shell, Source};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use green_tea_bare::fit::buddy::Buddy;
use green_tea_bare::fit::tlsf::Tlsf;
use green_tea_bare::fit::trace::{self, Event, Random, Report};
use green_tea_bare::fit::{Fit, Heap, Order, Policy};
use green_tea_bare::println;

/// The most events `trace on` records: 16 bytes each, so a mebibyte of the heap.
pub const TRACE_EVENTS: usize = 1 << 16;
/// The region each replay gets, from the heap.
const REGION: usize = 512 * 1024;
/// A block is long-lived if it outlives more than this many later allocations. A command
/// makes fewer than that, so a block that does was meant to outlive its command.
const LONG_LIVED: u32 = 64;

/// What the session's rounds do. Changing any of these changes every number `replay` prints.
const ROUNDS: usize = 12;
const SEED: u64 = 0x2545_f491_4f6c_dd1d;
const WORDS: [&str; 16] = [
    "a",
    "of",
    "the",
    "heap",
    "block",
    "free",
    "list",
    "first",
    "header",
    "footer",
    "policy",
    "coalesce",
    "allocator",
    "boundary",
    "fragmentation",
    "address-ordered",
];

const WAYS: [(Policy, &str, Order, &str); 6] = [
    (Policy::FirstFit, "first fit", Order::Lifo, "LIFO"),
    (Policy::FirstFit, "first fit", Order::Address, "address"),
    (Policy::NextFit, "next fit", Order::Lifo, "LIFO"),
    (Policy::NextFit, "next fit", Order::Address, "address"),
    (Policy::BestFit, "best fit", Order::Lifo, "LIFO"),
    (Policy::BestFit, "best fit", Order::Address, "address"),
];

/// A command line being written, on the stack: the script's own text isn't on the heap,
/// so the trace holds only what the commands allocate.
struct Line {
    bytes: [u8; 256],
    len: usize,
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl Line {
    /// Append between 1 and `most` words, chosen at random.
    fn words(&mut self, random: &mut Random, most: usize) {
        for _ in 0..1 + random.below(most) {
            let _ = write!(self, " {}", WORDS[random.below(WORDS.len())]);
        }
    }
}

/// Run the command `write` writes.
fn scripted(run: &mut impl FnMut(&str), write: impl FnOnce(&mut Line)) {
    let mut line = Line { bytes: [0; 256], len: 0 };
    write(&mut line);
    run(core::str::from_utf8(&line.bytes[..line.len]).unwrap_or(""));
}

impl Shell {
    /// `session`: a script of commands, run from the heap without their output. It works
    /// in rounds, as a person at a shell does: each round sets some variables, reads them,
    /// changes some, reads them again, and then unsets all but one in eight. Those stay.
    pub fn session(&mut self) {
        let (source, commands) = (self.source, self.commands);
        (self.source, self.quiet) = (Source::Heap, true);
        Shell::script(|line| _ = self.command(line));
        (self.source, self.quiet) = (source, false);
        println!(
            "session: {} commands in {ROUNDS} rounds, {} variables left set",
            self.commands - commands,
            self.vars.borrow().len()
        );
    }

    /// The session's script: `run` is called with each of its commands.
    pub fn script(mut run: impl FnMut(&str)) {
        let mut random = Random(SEED);
        for round in 0..ROUNDS {
            let count = 16 + random.below(48);
            for i in 0..count {
                scripted(&mut run, |line| {
                    let _ = write!(line, "set r{round}v{i}");
                    line.words(&mut random, 12);
                });
            }
            for pass in 0..2 {
                for _ in 0..count / (pass + 1) {
                    scripted(&mut run, |line| {
                        let _ = write!(line, "echo $r{round}v{}", random.below(count));
                        line.words(&mut random, 6);
                    });
                }
                for _ in 0..count / 3 * (1 - pass) {
                    scripted(&mut run, |line| {
                        let _ = write!(line, "set r{round}v{}", random.below(count));
                        line.words(&mut random, 12);
                    });
                }
            }
            for i in (0..count).filter(|i| i % 8 != 0) {
                scripted(&mut run, |line| {
                    let _ = write!(line, "unset r{round}v{i}");
                });
            }
        }
    }

    /// `replay`: replay the recorded trace every way, print what each way needed, and
    /// judge the result.
    pub fn replay(&mut self) {
        let Some(recording) = &self.trace else {
            return println!("nothing is recorded: `trace on`, some commands, `trace off`");
        };
        let events = &recording.events[..recording.len];
        let frees = events.iter().filter(|e| e.is_free()).count();
        if (events.len() - frees, frees) != self.counted && recording.dropped == 0 {
            return println!(
                "FAIL: the heap made {} allocations and {} frees while recording, and the trace has {} and {frees}",
                self.counted.0,
                self.counted.1,
                events.len() - frees
            );
        }
        match report(events, recording.dropped) {
            Ok(summary) => println!("PASS: {summary}"),
            Err(e) => println!("FAIL: {e}"),
        }
    }
}

fn report(events: &[Event], dropped: usize) -> Result<String, &'static str> {
    if dropped != 0 {
        return Err("the trace is incomplete: events were dropped");
    }
    let allocations = describe(events);
    let mut memory = vec![0u128; REGION / 16];
    let base = memory.as_mut_ptr() as usize;

    println!("replayed as recorded, each in a region of {} KiB:", REGION / 1024);
    let recorded = table(events, base)?;
    if recorded.iter().any(|r| r.allocations != allocations) {
        return Err("a replay didn't make every allocation of the trace");
    }

    println!("as recorded, against the allocators that don't search:");
    let others = others(events, base)?;
    if others.iter().any(|r| r.allocations != allocations || r.peak_live != recorded[0].peak_live) {
        return Err("a replay didn't make every allocation of the trace");
    }

    println!("the same sizes and the same lifetimes, dealt out at random:");
    let random = table(&trace::shuffled(events, SEED), base)?;

    split(events, base, &recorded, allocations)?;
    let (first, next) = (&recorded[1], &recorded[2]);
    Ok(format!(
        "{} events replayed six ways: a peak of {} live bytes needed {} (first fit by address) to {} (next fit, \
         LIFO); {} to {} when shuffled",
        events.len(),
        first.peak_live,
        first.footprint,
        next.footprint,
        random[1].footprint,
        random[2].footprint
    ))
}

/// Replay the long-lived blocks of `events` in one half of the region and the rest in the
/// other, each of the six ways, and print what the two halves needed next to what the
/// whole trace needed in one region.
fn split(events: &[Event], base: usize, recorded: &[Report], allocations: usize) -> Result<(), &'static str> {
    println!("as recorded, with the blocks that outlive {LONG_LIVED} allocations in a region of their own:");
    println!("  policy     order      long-lived   the rest      total   one region");
    let (long, short) = trace::by_lifetime(events, LONG_LIVED);
    for (i, (policy, policy_name, order, order_name)) in WAYS.into_iter().enumerate() {
        // SAFETY: the region is the caller's, and each half of it is used by one allocator.
        let long = trace::replay(&long, &mut unsafe { Fit::new(base, REGION / 2, policy, order) });
        let short = trace::replay(&short, &mut unsafe { Fit::new(base + REGION / 2, REGION / 2, policy, order) });
        println!(
            "  {policy_name:10} {order_name:8} {:12} {:10} {:10} {:11}",
            long.footprint,
            short.footprint,
            long.footprint + short.footprint,
            recorded[i].footprint
        );
        if long.failed + short.failed != 0 || long.allocations + short.allocations != allocations {
            return Err("splitting the trace by lifetime lost part of it");
        }
    }
    Ok(())
}

/// Print what the trace holds, and return how many allocations that is.
fn describe(events: &[Event]) -> usize {
    let mut sizes = BTreeMap::new();
    for event in events.iter().filter(|e| !e.is_free()) {
        *sizes.entry(event.size).or_insert(0usize) += 1;
    }
    let allocations: usize = sizes.values().sum();
    let bytes: usize = sizes.iter().map(|(size, count)| *size as usize * count).sum();
    println!(
        "the trace: {} events, {allocations} allocations of {bytes} bytes in all, {} frees",
        events.len(),
        events.len() - allocations
    );
    let mut common: Vec<(usize, u32)> = sizes.iter().map(|(size, count)| (*count, *size)).collect();
    common.sort_unstable_by(|a, b| b.cmp(a));
    let mut line = String::new();
    for (count, size) in common.iter().take(6) {
        let _ = write!(line, " {size} ({})", percent(*count, allocations));
    }
    println!(
        "sizes: {} different ones, from {} to {} bytes; the most common:{line}",
        sizes.len(),
        sizes.keys().next().copied().unwrap_or(0),
        sizes.keys().last().copied().unwrap_or(0)
    );
    let lifetimes = trace::lifetimes(events);
    let within = |limit: u32| lifetimes.iter().filter(|life| life.is_some_and(|life| life <= limit)).count();
    println!(
        "lifetimes: {} of the blocks are freed before 8 more are allocated, {} before {LONG_LIVED} more; {} are \
         never freed",
        percent(within(7), allocations),
        percent(within(LONG_LIVED), allocations),
        lifetimes.iter().filter(|life| life.is_none()).count()
    );
    allocations
}

/// Replay `events` each of the six ways in the region at `base`, and print a row for each.
/// The last three columns are the footprint's excess over the live bytes when it last
/// grew, over the most live bytes there ever were, and over the most bytes ever in blocks.
fn table(events: &[Event], base: usize) -> Result<Vec<Report>, &'static str> {
    println!("  policy     order     peak live  peak used  footprint  over live then  over peak live  over peak used");
    let mut reports: Vec<Report> = Vec::with_capacity(WAYS.len());
    for (policy, policy_name, order, order_name) in WAYS {
        // SAFETY: the region is the caller's, and this is the only allocator using it.
        let mut fit = unsafe { Fit::new(base, REGION, policy, order) };
        let r = trace::replay(events, &mut fit);
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
        if r.failed != 0 {
            return Err("a trace didn't fit in the region it was replayed in");
        }
        if r.peak_live >= r.peak_used || r.peak_used > r.footprint {
            return Err("a replay used less memory than the program asked for");
        }
        if reports.first().is_some_and(|first| {
            (r.peak_live, r.allocations, r.left) != (first.peak_live, first.allocations, first.left)
        }) {
            return Err("two replays of one trace disagree about what the program asked for");
        }
        reports.push(r);
    }
    Ok(reports)
}

/// Replay `events` against the buddy system and against TLSF, and print a row for each.
fn others(events: &[Event], base: usize) -> Result<Vec<Report>, &'static str> {
    println!("  allocator            peak live  peak used  footprint  over live then  over peak live  over peak used");
    let mut reports = Vec::with_capacity(2);
    for name in ["buddy system", "TLSF"] {
        // SAFETY: the region is the caller's, and one allocator uses it at a time.
        let r = unsafe {
            if name == "TLSF" {
                measured(events, &mut Tlsf::new(base, REGION))
            } else {
                measured(events, &mut Buddy::new(base, REGION))
            }
        }?;
        println!(
            "  {name:19} {:10} {:10} {:10} {:>15} {:>15} {:>15}",
            r.peak_live,
            r.peak_used,
            r.footprint,
            percent(r.footprint - r.live_at_peak_footprint, r.live_at_peak_footprint),
            percent(r.footprint - r.peak_live, r.peak_live),
            percent(r.footprint - r.peak_used, r.peak_used),
        );
        if r.failed != 0 {
            return Err("a trace didn't fit in the region it was replayed in");
        }
        if r.stats.longest_walk != 0 || (name == "TLSF" && r.stats.longest_search != 1) {
            return Err("an allocator that doesn't search searched");
        }
        reports.push(r);
    }
    Ok(reports)
}

/// Replay `events` against `heap`, and check the heap afterwards.
fn measured(events: &[Event], heap: &mut impl Heap) -> Result<Report, &'static str> {
    let report = trace::replay(events, heap);
    heap.check()?;
    Ok(report)
}

/// `part` as a percentage of `whole`, to one decimal place, without floating point.
fn percent(part: usize, whole: usize) -> String {
    let tenths = (part * 1000 + whole / 2) / whole.max(1);
    format!("{}.{}%", tenths / 10, tenths % 10)
}
