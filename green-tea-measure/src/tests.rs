use crate::oracle::{Explicit, Lifetimes, Profile};
use crate::replay::{self, Op, Ops, Table};
use crate::stats::{Summary, percentile, wins};
use crate::subjects::{self, Memory, NAMES};
use green_tea_fit::trace::{self, Event, Random};
use green_tea_fit::{Fit, Order, Policy};
use green_tea_lisp::programs::PROGRAMS;
use green_tea_lisp::{Heap, Kind, Lisp, Value};

const REGION: usize = 1 << 20;

/// 2,000 allocations of 1 to 3,000 bytes, most of them small, each freed at random later
/// or never.
fn workload() -> Vec<Event> {
    let mut random = Random(7);
    let mut events = Vec::new();
    let mut live = Vec::new();
    for n in 0..2000usize {
        let size = if random.below(10) == 0 { 1 + random.below(3000) } else { 1 + random.below(200) };
        let align = [1, 8, 16, 64][random.below(4)];
        events.push(Event::allocate(16 * (n + 1), size, align));
        live.push(16 * (n + 1));
        while live.len() > 300 || (!live.is_empty() && random.below(3) == 0) {
            events.push(Event::free(live.swap_remove(random.below(live.len()))));
        }
    }
    events
}

#[test]
fn a_trace_becomes_slots_that_are_reused() {
    let events =
        [Event::allocate(0xa0, 10, 8), Event::allocate(0xb0, 20, 8), Event::free(0xa0), Event::allocate(0xc0, 5, 8)];
    let ops = Ops::new(&events);
    assert_eq!(
        ops.ops,
        [
            Op { slot: 0, size: 10, align: 8 },
            Op { slot: 1, size: 20, align: 8 },
            Op { slot: 0, size: 0, align: 0 },
            Op { slot: 0, size: 5, align: 8 },
        ]
    );
    assert_eq!((ops.slots, ops.allocations, ops.frees, ops.total, ops.peak_live), (2, 3, 1, 35, 30));
}

#[test]
fn a_free_of_a_block_the_trace_never_allocated_is_left_out() {
    let ops = Ops::new(&[Event::free(0x90), Event::allocate(0xa0, 10, 8)]);
    assert_eq!((ops.len(), ops.unmatched), (1, 1));
}

#[test]
#[should_panic(expected = "allocated twice")]
fn a_trace_out_of_order_is_refused() {
    Ops::new(&[Event::allocate(0xa0, 10, 8), Event::allocate(0xa0, 10, 8)]);
}

/// The replay here and the replay of green-tea-fit are two programs. For the allocators
/// that both can drive, they must agree to the byte.
#[test]
fn the_replay_agrees_with_green_tea_fit() {
    let events = workload();
    let ops = Ops::new(&events);
    for (name, policy, order) in [
        ("first-lifo", Policy::FirstFit, Order::Lifo),
        ("first-addr", Policy::FirstFit, Order::Address),
        ("next-addr", Policy::NextFit, Order::Address),
        ("best-lifo", Policy::BestFit, Order::Lifo),
    ] {
        let mut subject = subjects::build(name, REGION, ops.total).unwrap();
        let space = replay::space(&ops, &mut *subject).unwrap();
        let memory = Memory::new(REGION);
        let mut fit = unsafe { Fit::new(memory.base(), memory.len(), policy, order) };
        let report = trace::replay(&events, &mut fit);
        assert_eq!(
            (space.peak_live, space.peak_used, space.footprint, space.refused),
            (report.peak_live, Some(report.peak_used), report.footprint, 0),
            "{name}"
        );
        let stats = report.stats;
        assert_eq!(space.searched, Some((stats.examined + stats.passed, stats.longest_search.max(stats.longest_walk))));
    }
}

/// The workload with no alignment over 16, which is all the buddy system does.
fn workload_for(name: &str) -> Vec<Event> {
    let mut events = workload();
    if name == "buddy" {
        events.iter_mut().for_each(|event| event.align = event.align.min(16));
    }
    events
}

#[test]
fn every_subject_replays_the_workload_and_hands_out_blocks_that_dont_overlap() {
    for name in NAMES {
        let ops = Ops::new(&workload_for(name));
        let mut subject = subjects::build(name, REGION, ops.total).unwrap();
        let space = replay::space(&ops, &mut *subject).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(space.refused, 0, "{name}");
        if name != "null" {
            assert!(space.footprint >= ops.peak_live, "{name}: a footprint below the live bytes");
        }
        // Again, by hand: every block is aligned, and is written to and read back while
        // every other block is live.
        subject.reset();
        let mut held: Vec<Option<(usize, usize, usize)>> = vec![None; ops.slots];
        for (n, op) in ops.ops.iter().enumerate() {
            if op.size == 0 {
                let (addr, size, align) = held[op.slot as usize].take().unwrap();
                if name != "null" {
                    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                    assert!(bytes.iter().all(|&b| b == addr as u8), "{name}: a block was overwritten while allocated");
                }
                unsafe { subject.free(addr, size, align) };
            } else {
                let (size, align) = (op.size as usize, op.align as usize);
                let addr = subject.allocate(size, align).unwrap_or_else(|| panic!("{name} refused event {n}"));
                if name != "null" {
                    assert!(addr.is_multiple_of(align), "{name}: a block of alignment {align} at {addr:x}");
                    unsafe { core::ptr::write_bytes(addr as *mut u8, addr as u8, size) };
                }
                held[op.slot as usize] = Some((addr, size, align));
            }
        }
        subject.check().unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn a_reset_subject_replays_to_the_same_numbers() {
    for name in NAMES {
        let ops = Ops::new(&workload_for(name));
        let mut subject = subjects::build(name, REGION, ops.total).unwrap();
        let first = replay::space(&ops, &mut *subject).unwrap();
        subject.reset();
        assert_eq!(replay::space(&ops, &mut *subject).unwrap(), first, "{name}");
    }
}

/// A block aligned to 64 bytes is placed by its address, so a footprint depends on where
/// the region starts. The C library puts a small buffer in one place and a large one in
/// another, at different offsets within a page.
#[test]
fn a_region_starts_on_a_page() {
    for len in [100, 5000, 100_000, 1 << 20, 1 << 24] {
        let memory = Memory::new(len);
        assert!(memory.base().is_multiple_of(4096), "a region of {len} bytes at {:x}", memory.base());
        assert!(memory.len() >= len && !memory.is_empty());
    }
}

#[test]
fn a_region_too_small_is_reported_and_not_measured() {
    let ops = Ops::new(&[Event::allocate(0xa0, 3000, 16), Event::free(0xa0)]);
    let mut subject = subjects::build("tlsf", 2048, ops.total).unwrap();
    assert_eq!(replay::space(&ops, &mut *subject).unwrap().refused, 1);
}

#[test]
fn the_three_replays_make_the_same_calls() {
    let ops = Ops::new(&workload());
    let mut table = Table::new(&ops);
    let mut subject = subjects::build("first-addr", REGION, ops.total).unwrap();
    let space = replay::space(&ops, &mut *subject).unwrap();
    subject.reset();
    assert!(replay::plain(&ops, &mut table, &mut *subject, 2));
    assert_eq!(subject.footprint(), space.footprint);
    assert_eq!(subject.searched(), space.searched);
    subject.reset();
    assert!(replay::pass(&ops, &mut table, &mut *subject, false).is_some());
    assert_eq!(subject.searched(), space.searched);
    subject.reset();
    let mut samples = Vec::new();
    assert!(replay::each(&ops, &mut table, &mut *subject, &mut samples, false));
    assert_eq!((samples.len(), subject.searched()), (ops.len(), space.searched));
    // Writing to the blocks changes nothing the allocator is asked, and leaves it in order.
    for name in NAMES.iter().filter(|name| **name != "null") {
        let ops = Ops::new(&workload_for(name));
        let mut table = Table::new(&ops);
        let mut subject = subjects::build(name, REGION, ops.total).unwrap();
        let space = replay::space(&ops, &mut *subject).unwrap();
        subject.reset();
        samples.clear();
        assert!(replay::each(&ops, &mut table, &mut *subject, &mut samples, true), "{name}");
        assert_eq!(subject.searched(), space.searched, "{name}");
        subject.check().unwrap_or_else(|e| panic!("{name}, after its blocks were written to: {e}"));
        subject.reset();
        assert!(replay::pass(&ops, &mut table, &mut *subject, true).is_some(), "{name}");
        subject.check().unwrap_or_else(|e| panic!("{name}, after its blocks were written to: {e}"));
    }
}

/// A subject that only counts its resets.
#[derive(Default)]
struct Resets(usize);

impl subjects::Subject for Resets {
    fn allocate(&mut self, _size: usize, _align: usize) -> Option<usize> {
        Some(16)
    }

    unsafe fn free(&mut self, _addr: usize, _size: usize, _align: usize) {}

    fn used(&self) -> Option<usize> {
        None
    }

    fn footprint(&self) -> usize {
        0
    }

    fn searched(&self) -> Option<(usize, usize)> {
        None
    }

    fn check(&self) -> Result<(), &'static str> {
        Ok(())
    }

    fn reset(&mut self) {
        self.0 += 1;
    }
}

/// `plain` is what callgrind counts, so a reset inside it is counted as the allocator's work.
#[test]
fn the_counted_loop_resets_only_between_passes() {
    let ops = Ops::new(&[Event::allocate(0xa0, 10, 8), Event::free(0xa0)]);
    let mut table = Table::new(&ops);
    for (passes, resets) in [(1, 0), (3, 2)] {
        let mut subject = Resets::default();
        assert!(replay::plain(&ops, &mut table, &mut subject, passes));
        assert_eq!(subject.0, resets, "{passes} passes");
    }
}

#[test]
fn a_percentile_is_a_sample() {
    let sorted: Vec<u32> = (1..=1000).collect();
    assert_eq!(percentile(&sorted, 50_000), 500);
    assert_eq!(percentile(&sorted, 99_000), 990);
    assert_eq!(percentile(&sorted, 99_900), 999);
    assert_eq!(percentile(&sorted, 100_000), 1000);
    assert_eq!(percentile(&sorted, 0), 1);
    assert_eq!(percentile(&[7u32], 99_900), 7);
    // Ten samples: the 99th percentile is the largest, because nine samples are 90%.
    assert_eq!(percentile(&[1u32, 1, 1, 1, 1, 1, 1, 1, 1, 100], 99_000), 100);
    assert_eq!(percentile(&[1u32, 1, 1, 1, 1, 1, 1, 1, 1, 100], 90_000), 1);
}

#[test]
fn a_tail_moves_the_mean_and_not_the_median() {
    let mut samples = vec![10u32; 999];
    samples.push(1_000_000);
    let s = Summary::of(&mut samples);
    assert_eq!((s.count, s.min, s.median, s.p90, s.p99, s.p999, s.max), (1000, 10, 10, 10, 10, 10, 1_000_000));
    assert!((s.mean - 1009.99).abs() < 0.001);
}

#[test]
fn wins_counts_pairs() {
    assert_eq!(wins(&[1, 5], &[2, 3, 4]), (3, 6));
    assert_eq!(wins(&[9], &[9]), (0, 1));
}

/// Three pairs: the first is kept in the roots, the second is dropped at once, the third
/// is reachable only through the first.
#[test]
fn the_profile_finds_an_object_dead_at_the_allocation_after_it_became_unreachable() {
    let mut heap = Profile::new();
    let pair = |heap: &mut Profile| {
        heap.push(Value::int(1));
        heap.push(Value::NIL);
        heap.alloc(Kind::Pair).unwrap();
    };
    pair(&mut heap); // object 0, kept
    pair(&mut heap); // object 1
    heap.pop(); // nothing reaches object 1 now
    pair(&mut heap); // object 2: object 1 is found dead here
    let third = heap.top();
    heap.set_field(heap.root(0).as_obj().unwrap(), 1, third);
    heap.pop(); // object 2 is still reached, through object 0
    pair(&mut heap); // object 3
    heap.pop();
    heap.pop(); // nothing reaches anything
    pair(&mut heap); // object 4: objects 0, 2 and 3 are found dead here
    let made = heap.finish();
    assert_eq!(made.deaths, [vec![], vec![], vec![1], vec![], vec![0, 2, 3]]);
    assert_eq!((made.left, made.peak_words, made.allocations()), (1, 9, 5));
}

/// Dead objects are found only before an allocation, so one that dies after the last
/// allocation must not be counted as reachable at the end.
#[test]
fn an_object_that_dies_after_the_last_allocation_is_not_left() {
    let mut heap = Profile::new();
    heap.push(Value::int(1));
    heap.push(Value::NIL);
    heap.alloc(Kind::Pair).unwrap();
    heap.pop(); // nothing reaches it now, and nothing is allocated after this
    let made = heap.finish();
    assert_eq!((made.left, made.died_after_last), (0, 1));
}

fn lifetimes(source: &str) -> Lifetimes {
    let mut lisp = Lisp::new(Profile::new());
    lisp.run(source).unwrap();
    lisp.heap.finish()
}

/// The second run. In a debug build, which a test is, `Explicit` panics if the
/// interpreter uses an object whose block the lifetimes have had freed.
#[test]
fn every_program_computes_its_value_with_explicit_frees() {
    for program in &PROGRAMS {
        let made = lifetimes(program.source);
        let died: usize = made.deaths.iter().map(Vec::len).sum();
        assert_eq!(died + made.died_after_last + made.left, made.allocations(), "{}", program.name);
        for name in ["tlsf", "first-addr", "best-lifo", "buddy", "pools", "slabs", "caches", "shards", "hardened"] {
            let subject = subjects::build(name, REGION, 32 * made.allocations()).unwrap();
            let mut lisp = Lisp::new(Explicit::new(subject, Some(&made)));
            assert_eq!(lisp.run(program.source).as_deref(), Ok(program.value), "{} on {name}", program.name);
            let stats = lisp.heap.stats();
            assert_eq!((stats.allocated as usize, stats.freed as usize), (made.allocations(), died));
            assert_eq!(stats.peak_words, made.peak_words, "{} on {name}", program.name);
            lisp.heap.subject().check().unwrap();
        }
    }
}

/// A cycle is garbage that counting can't free. The oracle frees it: `ring` leaves 800
/// objects behind on the counting heap, and fewer here.
#[test]
fn the_oracle_frees_cycles() {
    let ring = PROGRAMS.iter().find(|p| p.name == "ring").unwrap();
    let made = lifetimes(ring.source);
    assert!(made.left < 800, "{} objects were still reachable at the end", made.left);
}

/// The lifetimes as a trace, and the run with explicit frees, are two accounts of the same
/// allocations and frees. An allocator must need the same bytes for both.
#[test]
fn the_lifetimes_replayed_as_a_trace_need_what_the_run_needs() {
    for program in &PROGRAMS {
        let made = lifetimes(program.source);
        let ops = Ops::new(&made.events());
        for name in ["tlsf", "first-addr", "pools"] {
            let mut subject = subjects::build(name, REGION, ops.total).unwrap();
            let space = replay::space(&ops, &mut *subject).unwrap();
            let mut lisp = Lisp::new(Explicit::new(subjects::build(name, REGION, ops.total).unwrap(), Some(&made)));
            lisp.run(program.source).unwrap();
            assert_eq!(lisp.heap.subject().footprint(), space.footprint, "{} on {name}", program.name);
        }
    }
}

#[test]
#[should_panic(expected = "after the oracle freed it")]
fn lifetimes_that_free_an_object_too_early_are_caught() {
    let list = PROGRAMS.iter().find(|p| p.name == "list").unwrap();
    let mut made = lifetimes(list.source);
    // Every death one allocation early, for the objects that exist by then.
    for n in 1..made.deaths.len() {
        let (early, on_time): (Vec<u32>, Vec<u32>) = made.deaths[n].iter().partition(|&&dead| (dead as usize) < n - 1);
        made.deaths[n] = on_time;
        made.deaths[n - 1].extend(early);
    }
    let subject = subjects::build("tlsf", REGION, 32 * made.allocations()).unwrap();
    let _ = Lisp::new(Explicit::new(subject, Some(&made))).run(list.source);
}
