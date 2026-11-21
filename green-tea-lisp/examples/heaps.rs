//! Two questions about the heaps of green-tea-lisp that are answered on the host, because
//! one takes thousands of runs and the other a clock that isn't an emulator's:
//!
//!     cargo run --release -p green-tea-lisp --example heaps
//!
//! First, the smallest heap each of the five programs computes its value in, on each
//! heap, found by trying every size from one word up. Then one collection of a heap that
//! holds a list to keep and a list of garbage, timed.

use green_tea_lisp::gc::{Collect, Counts};
use green_tea_lisp::mark::{MarkHeap, Reclaim};
use green_tea_lisp::programs::PROGRAMS;
use green_tea_lisp::rc::{Cycles, Freeing, RcHeap};
use green_tea_lisp::semispace::Semispace;
use green_tea_lisp::{Heap, Kind, Lisp, Value};
use std::sync::OnceLock;
use std::time::Instant;

const MARK_STACK: usize = 1024;
/// How far above the smallest heap to look for sizes that don't work.
const BEYOND: usize = 400;

fn fits<H: Heap>(heap: H, source: &str, value: &str) -> bool {
    Lisp::new(heap).run(source).as_deref() == Ok(value)
}

/// The smallest size `run` succeeds with, and the sizes just above it where it doesn't.
fn smallest(run: impl Fn(usize) -> bool) -> (usize, Vec<usize>) {
    let mut words = 1;
    while !run(words) {
        words += 1;
    }
    (words, (words..words + BEYOND).filter(|&w| !run(w)).collect())
}

/// What the collections of a run counted.
fn counts<H: Collect>(heap: H, source: &str) -> Counts {
    let mut lisp = Lisp::new(heap);
    lisp.run(source).unwrap();
    lisp.heap.counts()
}

fn nanoseconds() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Push a list of `n` integers.
fn list<H: Heap>(heap: &mut H, n: i64) {
    heap.push(Value::NIL);
    for i in 0..n {
        heap.push(Value::int(i));
        heap.push(heap.root(heap.roots() - 2));
        heap.alloc(Kind::Pair).unwrap();
        heap.set_root(heap.roots() - 2, heap.top());
        heap.pop();
    }
}

/// A heap with `garbage` pairs that nothing refers to and then `live` pairs in a list,
/// collected once.
fn one_collection<H: Collect>(mut heap: H, name: &str, live: i64, garbage: i64) {
    list(&mut heap, garbage);
    heap.pop();
    list(&mut heap, live);
    heap.set_clock(nanoseconds);
    heap.collect();
    let counts = heap.counts();
    assert_eq!((counts.collections, heap.stats().live()), (1, live as u64));
    println!(
        "  {name:13} {live:7} {garbage:8} {:8} {:8} {:12} {:13}",
        counts.traced,
        counts.walked,
        counts.moved_words,
        counts.pause_total / 1000
    );
}

fn main() {
    println!("the smallest heap each program computes its value in, in words:");
    println!("  program   counting  with trial deletion  mark-sweep  mark-compact  copying");
    let mut holes = Vec::new();
    let mut collections = Vec::new();
    for p in &PROGRAMS {
        let trial = Cycles::Collect { every: usize::MAX };
        let found = [
            smallest(|w| fits(RcHeap::new(w, Freeing::Recursive, Cycles::Leak), p.source, p.value)),
            smallest(|w| fits(RcHeap::new(w, Freeing::Recursive, trial), p.source, p.value)),
            smallest(|w| fits(MarkHeap::new(w, Reclaim::Sweep, MARK_STACK), p.source, p.value)),
            smallest(|w| fits(MarkHeap::new(w, Reclaim::Compact, MARK_STACK), p.source, p.value)),
            smallest(|w| fits(Semispace::new(w), p.source, p.value)),
        ];
        println!(
            "  {:8} {:9} {:20} {:11} {:13} {:8}",
            p.name, found[0].0, found[1].0, found[2].0, found[3].0, found[4].0
        );
        let names = ["counting", "with trial deletion", "mark-sweep", "mark-compact", "copying"];
        for (name, (words, failed)) in names.iter().zip(&found) {
            if !failed.is_empty() {
                holes.push(format!("  {:8} {name}, smallest {words}: {failed:?}", p.name));
            }
        }
        for multiple in [1, 2, 4, 8] {
            collections.push(format!(
                "  {:8} {multiple:8} {:11} {:9} {:13} {:9} {:8} {:9}",
                p.name,
                counts(MarkHeap::new(multiple * found[2].0, Reclaim::Sweep, MARK_STACK), p.source).collections,
                counts(MarkHeap::new(multiple * found[2].0, Reclaim::Sweep, MARK_STACK), p.source).traced,
                counts(MarkHeap::new(multiple * found[3].0, Reclaim::Compact, MARK_STACK), p.source).collections,
                counts(MarkHeap::new(multiple * found[3].0, Reclaim::Compact, MARK_STACK), p.source).traced,
                counts(Semispace::new(multiple * found[4].0), p.source).collections,
                counts(Semispace::new(multiple * found[4].0), p.source).traced,
            ));
        }
    }
    println!("sizes above the smallest, and less than {BEYOND} words above it, where the program doesn't fit:");
    for line in holes {
        println!("{line}");
    }
    println!("collections, and objects traced, in a heap that is a multiple of the smallest:");
    println!("  program  multiple  mark-sweep    traced  mark-compact    traced  copying    traced");
    for line in collections {
        println!("{line}");
    }

    for run in 1..=3 {
        println!("one collection, run {run}; the last column is a time, and varies from run to run:");
        println!("  heap             live  garbage   traced   walked  words moved  microseconds");
        for (live, garbage) in [(1_000, 100_000), (10_000, 100_000), (100_000, 100_000), (100_000, 1_000)] {
            let words = 3 * (live + garbage) as usize;
            one_collection(MarkHeap::new(words, Reclaim::Sweep, MARK_STACK), "mark-sweep", live, garbage);
            one_collection(MarkHeap::new(words, Reclaim::Compact, MARK_STACK), "mark-compact", live, garbage);
            one_collection(Semispace::new(2 * words), "copying", live, garbage);
        }
    }
}
