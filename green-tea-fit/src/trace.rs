//! Allocation traces: what a program asked of its allocator, in order, and what replaying
//! that against an allocator costs.
//!
//! A trace is a list of [`Event`]s. Each is an address and a size: a block of that size
//! was allocated at that address, or, if the size is 0, the block at that address was
//! freed. The addresses are the ones the program got when the trace was recorded. A replay
//! uses them only to tell which block a free means.
//!
//! [`replay`] runs a trace against a [`Fit`] and returns a [`Report`]: how much the program
//! had allocated at its peak, and how much of the region the allocator needed for it.
//! [`shuffled`] makes the trace that a random-number generator would have produced from the
//! same sizes and lifetimes, and [`by_lifetime`] splits a trace in two, the blocks that
//! live long and the rest.

use crate::{Heap, Stats};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// One allocation or one free.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Event {
    /// The address the program was given, or gave back.
    pub addr: usize,
    /// The bytes asked for, or 0 for a free.
    pub size: u32,
    /// The alignment asked for, or 0 for a free.
    pub align: u32,
}

impl Event {
    pub fn allocate(addr: usize, size: usize, align: usize) -> Event {
        Event { addr, size: size as u32, align: align as u32 }
    }

    pub fn free(addr: usize) -> Event {
        Event { addr, size: 0, align: 0 }
    }

    pub fn is_free(&self) -> bool {
        self.size == 0
    }
}

/// What replaying a trace against an allocator came to. *Live* is the bytes the program
/// asked for and hasn't freed; *used* is the bytes in the blocks that hold them, tags and
/// rounding included; the *footprint* is [`Fit::footprint`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Report {
    pub allocations: usize,
    pub frees: usize,
    /// Allocations the allocator refused: the region was too small for the trace.
    pub failed: usize,
    /// Frees of blocks that the trace never allocated, which a replay skips.
    pub unmatched: usize,
    /// The most live bytes at once, and the used bytes and the footprint at that moment.
    pub peak_live: usize,
    pub used_at_peak_live: usize,
    pub footprint_at_peak_live: usize,
    /// The most used bytes at once.
    pub peak_used: usize,
    /// The footprint at the end, which is the largest it ever was, and the live and used
    /// bytes at the moment it last grew.
    pub footprint: usize,
    pub live_at_peak_footprint: usize,
    pub used_at_peak_footprint: usize,
    /// The live bytes and the footprint after each event, added up: their ratio is the
    /// footprint per live byte, averaged over the run.
    pub live_sum: u64,
    pub footprint_sum: u64,
    /// Blocks still allocated at the end.
    pub left: usize,
    pub stats: Stats,
}

/// Replay `events` against `fit`, which should be new.
pub fn replay(events: &[Event], fit: &mut (impl Heap + ?Sized)) -> Report {
    let mut report = Report::default();
    // Where each block of the trace is now: the address recorded, to the address in `fit`
    // and the size asked for.
    let mut blocks = BTreeMap::new();
    let mut live = 0;
    for event in events {
        if event.is_free() {
            let Some((addr, size)) = blocks.remove(&event.addr) else {
                report.unmatched += 1;
                continue;
            };
            // SAFETY: `addr` came from `fit.allocate`, and leaves `blocks` as it is freed.
            unsafe { fit.free(addr) }.expect("a replayed free was refused");
            live -= size;
            report.frees += 1;
        } else {
            let size = event.size as usize;
            let Some(addr) = fit.allocate(size, event.align as usize) else {
                report.failed += 1;
                continue;
            };
            blocks.insert(event.addr, (addr, size));
            live += size;
            report.allocations += 1;
            if live > report.peak_live {
                report.peak_live = live;
                report.used_at_peak_live = fit.used();
                report.footprint_at_peak_live = fit.footprint();
            }
            if fit.footprint() > report.footprint {
                report.footprint = fit.footprint();
                report.live_at_peak_footprint = live;
                report.used_at_peak_footprint = fit.used();
            }
        }
        report.live_sum += live as u64;
        report.footprint_sum += fit.footprint() as u64;
    }
    report.peak_used = fit.stats().peak;
    report.left = blocks.len();
    report.stats = fit.stats();
    report
}

/// How long each block of a trace lived, counted in allocations: the number of blocks
/// allocated between its allocation and its free. `None` for a block the trace never
/// frees. One entry per allocation, in the trace's order.
pub fn lifetimes(events: &[Event]) -> Vec<Option<u32>> {
    let mut lifetimes = Vec::new();
    let mut born = BTreeMap::new(); // the address recorded, to which allocation it was
    for event in events {
        if event.is_free() {
            if let Some(index) = born.remove(&event.addr) {
                lifetimes[index] = Some((lifetimes.len() - index - 1) as u32);
            }
        } else {
            born.insert(event.addr, lifetimes.len());
            lifetimes.push(None);
        }
    }
    lifetimes
}

/// A trace with the same sizes and the same lifetimes as `events`, dealt out at random:
/// each allocation gets the size of one allocation of the original and the lifetime of
/// another. What a block's size says about how long it will live, and what one allocation
/// says about the next, are gone. (A block whose lifetime would take it past the end of the
/// trace is freed at the end.) The same `seed` gives the same trace.
pub fn shuffled(events: &[Event], seed: u64) -> Vec<Event> {
    let mut sizes: Vec<(u32, u32)> = events.iter().filter(|e| !e.is_free()).map(|e| (e.size, e.align)).collect();
    let mut lifetimes = lifetimes(events);
    let mut random = Random(seed);
    random.shuffle(&mut sizes);
    random.shuffle(&mut lifetimes);

    // Block `i` is allocated `i`th and freed just before allocation `i + lifetime + 1`.
    let mut deaths: Vec<(usize, usize)> =
        lifetimes.iter().enumerate().filter_map(|(i, life)| Some((i + (*life)? as usize + 1, i))).collect();
    deaths.sort_unstable();
    let mut deaths = deaths.into_iter().peekable();
    let mut out = Vec::with_capacity(events.len());
    let addr = |i: usize| (i + 1) * 16; // any address will do, as long as no two blocks share one
    for (i, &(size, align)) in sizes.iter().enumerate() {
        while let Some((_, dead)) = deaths.next_if(|&(at, _)| at <= i) {
            out.push(Event::free(addr(dead)));
        }
        out.push(Event { addr: addr(i), size, align });
    }
    out.extend(deaths.map(|(_, dead)| Event::free(addr(dead))));
    out
}

/// Split a trace in two: the blocks that outlive more than `threshold` later allocations,
/// or are never freed, and the rest. Each half keeps its events in their original order.
pub fn by_lifetime(events: &[Event], threshold: u32) -> (Vec<Event>, Vec<Event>) {
    let lifetimes = lifetimes(events);
    let (mut long, mut short) = (Vec::new(), Vec::new());
    let mut is_long = BTreeMap::new(); // for each live block, which half it went to
    let mut index = 0;
    for event in events {
        if event.is_free() {
            match is_long.remove(&event.addr) {
                Some(true) => long.push(*event),
                Some(false) => short.push(*event),
                None => {}
            }
        } else {
            let long_lived = lifetimes[index].is_none_or(|life| life > threshold);
            index += 1;
            is_long.insert(event.addr, long_lived);
            if long_lived { long.push(*event) } else { short.push(*event) }
        }
    }
    (long, short)
}

/// A linear congruential generator (Knuth's MMIX constants): the same seed gives the same
/// numbers on every architecture.
pub struct Random(pub u64);

impl Random {
    /// A number below `bound`.
    pub fn below(&mut self, bound: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % bound
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Event, Random, by_lifetime, lifetimes, replay, shuffled};
    use crate::{Fit, Order, Policy};
    use std::vec;
    use std::vec::Vec;

    fn with_fit<R>(len: usize, policy: Policy, order: Order, f: impl FnOnce(&mut Fit) -> R) -> R {
        let mut memory = vec![0u128; len / 16];
        let mut fit = unsafe { Fit::new(memory.as_mut_ptr() as usize, len, policy, order) };
        f(&mut fit)
    }

    /// A trace of `count` allocations with up to `live` blocks alive at once.
    fn random_trace(count: usize, live: usize, seed: u64) -> Vec<Event> {
        let mut random = Random(seed);
        let (mut events, mut alive) = (Vec::new(), Vec::new());
        for i in 0..count {
            if alive.len() == live || (!alive.is_empty() && random.below(3) == 0) {
                let addr = alive.swap_remove(random.below(alive.len()));
                events.push(Event::free(addr));
            }
            let addr = 0x1000 + i * 16;
            events.push(Event::allocate(addr, 1 + random.below(300), 8));
            alive.push(addr);
        }
        events
    }

    #[test]
    fn a_replay_counts_what_the_program_asked_for() {
        // a = 100, b = 50, free a, c = 30, free c, free b.
        let events = [
            Event::allocate(0xa0, 100, 8),
            Event::allocate(0xb0, 50, 8),
            Event::free(0xa0),
            Event::allocate(0xc0, 30, 8),
            Event::free(0xc0),
            Event::free(0xb0),
        ];
        let report = with_fit(4096, Policy::FirstFit, Order::Address, |fit| replay(&events, fit));
        assert_eq!((report.allocations, report.frees, report.failed, report.left), (3, 3, 0, 0));
        assert_eq!(report.peak_live, 150);
        assert_eq!(report.peak_used, 128 + 80); // 100 and 50 bytes, as blocks
        assert_eq!(report.footprint, 208); // c fits where a was
        assert_eq!(report.live_at_peak_footprint, 150);
        assert_eq!(report.live_sum, 100 + 150 + 50 + 80 + 50);
    }

    #[test]
    fn a_hole_too_small_for_the_next_request_raises_the_footprint() {
        // Free a block of 100 between two others, then ask for 200.
        let events = [
            Event::allocate(0xa0, 100, 8),
            Event::allocate(0xb0, 100, 8),
            Event::allocate(0xc0, 100, 8),
            Event::free(0xb0),
            Event::allocate(0xd0, 200, 8),
        ];
        let report = with_fit(4096, Policy::BestFit, Order::Address, |fit| replay(&events, fit));
        assert_eq!(report.peak_live, 400);
        assert_eq!(report.peak_used, 2 * 128 + 224);
        assert_eq!(report.footprint, 3 * 128 + 224); // the hole is still there
        assert_eq!(report.left, 3);
    }

    #[test]
    fn frees_of_blocks_the_trace_never_allocated_are_skipped() {
        let events = [Event::free(0x50), Event::allocate(0xa0, 10, 8), Event::free(0xa0), Event::free(0xa0)];
        let report = with_fit(4096, Policy::FirstFit, Order::Lifo, |fit| replay(&events, fit));
        assert_eq!((report.allocations, report.frees, report.unmatched), (1, 1, 2));
    }

    #[test]
    fn a_region_too_small_for_the_trace_is_reported() {
        let events = [Event::allocate(0xa0, 200, 8), Event::allocate(0xb0, 200, 8), Event::free(0xb0)];
        let report = with_fit(320, Policy::FirstFit, Order::Lifo, |fit| replay(&events, fit));
        assert_eq!((report.allocations, report.failed, report.unmatched), (1, 1, 1));
    }

    #[test]
    fn lifetimes_count_the_allocations_a_block_outlives() {
        let events = [
            Event::allocate(0xa0, 1, 8),
            Event::allocate(0xb0, 1, 8),
            Event::free(0xb0),
            Event::allocate(0xc0, 1, 8),
            Event::allocate(0xb0, 1, 8), // the address of b, reused
            Event::free(0xa0),
        ];
        assert_eq!(lifetimes(&events), [Some(3), Some(0), None, None]);
    }

    #[test]
    fn a_shuffled_trace_has_the_same_sizes_and_lifetimes() {
        let events = random_trace(2000, 50, 7);
        let other = shuffled(&events, 1);
        assert_ne!(events, other);
        assert_eq!(shuffled(&events, 1), other);
        let sorted = |events: &[Event]| {
            let mut sizes: Vec<u32> = events.iter().filter(|e| !e.is_free()).map(|e| e.size).collect();
            let mut lives = lifetimes(events);
            sizes.sort();
            lives.sort();
            (sizes, lives)
        };
        let ((sizes, lives), (other_sizes, other_lives)) = (sorted(&events), sorted(&other));
        assert_eq!(sizes, other_sizes);
        // The same lifetimes, except those cut short by the end of the trace.
        assert_eq!(lives.iter().filter(|l| l.is_none()).count(), other_lives.iter().filter(|l| l.is_none()).count());
        assert!(other_lives.iter().zip(&lives).all(|(other, life)| other <= life));
        for policy in [Policy::FirstFit, Policy::NextFit, Policy::BestFit] {
            let report = with_fit(1 << 20, policy, Order::Address, |fit| replay(&other, fit));
            assert_eq!((report.failed, report.unmatched, report.allocations), (0, 0, 2000));
        }
    }

    #[test]
    fn a_trace_split_by_lifetime_loses_nothing() {
        let events = random_trace(2000, 50, 3);
        let (long, short) = by_lifetime(&events, 20);
        assert_eq!(long.len() + short.len(), events.len());
        assert!(!long.is_empty() && !short.is_empty());
        // Every block of the short half died within 20 allocations of the whole trace, so
        // within 20 of its own half.
        assert!(lifetimes(&short).iter().all(|life| life.is_some_and(|life| life <= 20)));
        for half in [&long, &short] {
            let report = with_fit(1 << 20, Policy::FirstFit, Order::Address, |fit| replay(half, fit));
            assert_eq!((report.failed, report.unmatched), (0, 0));
        }
    }

    #[test]
    fn the_footprint_is_never_less_than_what_is_used() {
        let events = random_trace(5000, 100, 11);
        for policy in [Policy::FirstFit, Policy::NextFit, Policy::BestFit] {
            for order in [Order::Lifo, Order::Address] {
                let report = with_fit(1 << 20, policy, order, |fit| {
                    let report = replay(&events, fit);
                    fit.check().unwrap();
                    report
                });
                assert_eq!(report.failed, 0);
                assert!(report.peak_live < report.peak_used && report.peak_used <= report.footprint);
                assert!(report.footprint_at_peak_live <= report.footprint);
                assert!(report.live_at_peak_footprint <= report.peak_live);
            }
        }
    }
}
