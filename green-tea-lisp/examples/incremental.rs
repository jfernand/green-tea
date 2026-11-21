//! The incremental heap of green-tea-lisp, measured on the host, where forty runs of
//! seven programs take a second:
//!
//!     cargo run --release -p green-tea-lisp --example incremental
//!
//! Every number printed is a count, and is the same on every run. The pace by ticks is
//! given a tick at every so many allocations, in place of a timer.

use green_tea_lisp::gc::{Collect, Counts};
use green_tea_lisp::incremental::{Barrier, Birth, IncHeap, MUTATORS, Pace, Steps};
use green_tea_lisp::mark::{MarkHeap, Reclaim};
use green_tea_lisp::programs::{PROGRAMS, Program};
use green_tea_lisp::{Heap, Lisp, Stats};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};

const WORDS: usize = 4096;
const MARK_STACK: usize = 1024;

struct Outcome {
    value: Result<String, String>,
    stats: Stats,
    counts: Counts,
    steps: Steps,
}

fn run(heap: IncHeap, program: &Program, stress: bool) -> Outcome {
    let mut lisp = Lisp::new(heap);
    lisp.heap.set_audit(true);
    lisp.heap.set_stress(stress);
    // Without a barrier the heap frees objects in use, and panics when one is touched.
    let value = match catch_unwind(AssertUnwindSafe(|| lisp.run(program.source))) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{error:?}")),
        Err(panic) => Err(panic.downcast_ref::<String>().cloned().unwrap_or_else(|| String::from("a panic"))),
    };
    Outcome { value, stats: lisp.heap.stats(), counts: lisp.heap.counts(), steps: lisp.heap.steps() }
}

fn heap(barrier: Barrier, birth: Birth, pace: Pace) -> IncHeap {
    IncHeap::new(WORDS, barrier, pace, MARK_STACK).born(birth)
}

fn whole() -> IncHeap {
    heap(Barrier::Deletion, Birth::Black, Pace::Whole)
}

fn deletion(units: usize) -> IncHeap {
    heap(Barrier::Deletion, Birth::Black, Pace::Alloc(units))
}

fn programs() -> impl Iterator<Item = &'static Program> {
    PROGRAMS.iter().chain(&MUTATORS)
}

fn checked(outcome: Outcome, program: &Program) -> Outcome {
    assert_eq!(outcome.value.as_deref(), Ok(program.value), "{}", program.name);
    assert_eq!(outcome.steps.lost, 0, "{}", program.name);
    outcome
}

/// A tick at every `EVERY`th allocation.
static EVERY: AtomicU64 = AtomicU64::new(1);
static ASKED: AtomicU64 = AtomicU64::new(0);

fn ticker() -> bool {
    (ASKED.fetch_add(1, Ordering::Relaxed) + 1).is_multiple_of(EVERY.load(Ordering::Relaxed))
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));

    println!("seven programs in {WORDS} words: the longest stop, in units of work");
    println!("                         mark-sweep     a cycle whole       a cycle in steps of 4 units an allocation");
    println!("  program   allocated  collections  most  cycles  most     cycles  steps  most  stalls  most root slots");
    for program in programs() {
        let mut lisp = Lisp::new(MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK));
        assert_eq!(lisp.run(program.source).as_deref(), Ok(program.value));
        let old = lisp.heap.counts();
        let one = checked(run(whole(), program, false), program);
        let steps = checked(run(deletion(4), program, false), program);
        println!(
            "  {:8} {:10} {:12} {:5} {:7} {:5} {:10} {:6} {:5} {:7} {:16}",
            program.name,
            one.stats.allocated,
            old.collections,
            old.largest_work,
            one.counts.collections,
            one.steps.largest,
            steps.counts.collections,
            steps.steps.steps,
            steps.steps.largest,
            steps.steps.stalls,
            steps.steps.most_roots
        );
    }

    println!();
    println!("the same runs: the work of the cycles, in objects");
    println!("            a cycle whole     a cycle in steps of 4 units an allocation (deletion barrier)");
    println!("  program   marked   swept    marked   swept  born marked  kept as garbage  most in one cycle");
    for program in programs() {
        let one = checked(run(whole(), program, false), program);
        let steps = checked(run(deletion(4), program, false), program);
        println!(
            "  {:8} {:7} {:7} {:9} {:7} {:12} {:16} {:18}",
            program.name,
            one.counts.traced,
            one.counts.walked,
            steps.counts.traced,
            steps.counts.walked,
            steps.steps.born_marked,
            steps.steps.floating,
            steps.steps.largest_floating
        );
    }

    println!();
    println!("the same programs, 4 units an allocation: what the write barrier did");
    println!("                    insertion, born white           insertion, born black           deletion");
    println!(
        "  program   stores  ran  shaded  root scans  slots  ran  shaded  root scans  slots  ran  shaded  root scans  slots"
    );
    for program in programs() {
        let ways = [
            heap(Barrier::Insertion, Birth::White, Pace::Alloc(4)),
            heap(Barrier::Insertion, Birth::Black, Pace::Alloc(4)),
            deletion(4),
        ]
        .map(|heap| checked(run(heap, program, false), program));
        print!("  {:8} {:7}", program.name, ways[0].steps.stores);
        for way in &ways {
            print!(" {:4} {:7} {:11} {:6}", way.steps.barriers, way.steps.shaded, way.steps.scans, way.counts.roots);
        }
        println!();
    }

    println!();
    println!("the same runs: garbage that a cycle kept, in objects");
    println!("            insertion, born white           insertion, born black           deletion");
    println!("  program   cycles  stalls  kept  most      cycles  stalls  kept  most      cycles  stalls  kept  most");
    for program in programs() {
        print!("  {:8}", program.name);
        for heap in [
            heap(Barrier::Insertion, Birth::White, Pace::Alloc(4)),
            heap(Barrier::Insertion, Birth::Black, Pace::Alloc(4)),
            deletion(4),
        ] {
            let way = checked(run(heap, program, false), program);
            print!(
                " {:7} {:7} {:5} {:5}     ",
                way.counts.collections, way.steps.stalls, way.steps.floating, way.steps.largest_floating
            );
        }
        println!();
    }

    let alist = &PROGRAMS[2];
    println!();
    println!("alist, deletion barrier, a reserve of 512 objects: the pace, in units at each allocation");
    println!("   units  cycles   steps  most units in a step  stalls  most units in a stall  kept as garbage");
    for units in [1, 2, 3, 4, 8, 16, 64, 256] {
        let way = checked(run(deletion(units), alist, false), alist);
        println!(
            "  {units:6} {:7} {:7} {:21} {:7} {:22} {:16}",
            way.counts.collections,
            way.steps.steps,
            way.steps.largest,
            way.steps.stalls,
            way.steps.largest_stall,
            way.steps.floating
        );
    }

    println!();
    println!("alist, deletion barrier: stalls, by pace and by reserve");
    println!("  reserve   1 unit  2 units  3 units  4 units  8 units  16 units");
    for reserve in [64, 128, 256, 512, 1024] {
        print!("  {reserve:7}");
        for units in [1, 2, 3, 4, 8, 16] {
            let way = checked(run(deletion(units).reserving(reserve), alist, false), alist);
            print!(" {:8}", way.steps.stalls);
        }
        println!();
    }

    println!();
    println!("alist, deletion barrier: the same work in fewer, longer steps (a tick at every so many allocations)");
    println!("  a tick every  units a tick  cycles  steps  most units in a step  stalls  kept as garbage");
    for every in [1, 4, 16, 64, 256] {
        EVERY.store(every, Ordering::Relaxed);
        ASKED.store(0, Ordering::Relaxed);
        let mut heap = heap(Barrier::Deletion, Birth::Black, Pace::Tick(4 * every as usize));
        heap.set_ticker(ticker);
        let way = checked(run(heap, alist, false), alist);
        println!(
            "  {every:12} {:13} {:7} {:6} {:21} {:7} {:16}",
            4 * every,
            way.counts.collections,
            way.steps.steps,
            way.steps.largest,
            way.steps.stalls,
            way.steps.floating
        );
    }

    println!();
    println!("no barrier, 4 units an allocation:");
    println!("  program   reachable objects freed  what the program did");
    for program in programs() {
        let way = run(heap(Barrier::None, Birth::Black, Pace::Alloc(4)), program, false);
        let did = match &way.value {
            Ok(value) if value == program.value => format!("computed {value}"),
            Ok(value) => format!("computed {value}, and should have computed {}", program.value),
            Err(error) => format!("stopped: {error}"),
        };
        println!("  {:8} {:24}  {did}", program.name, way.steps.lost);
    }

    let list = &PROGRAMS[0];
    println!();
    println!("list in 65536 words, with a cycle started whenever none is in progress: objects born white and black");
    println!("  barrier and birth   units  cycles finished  scans of the root stack  root slots read  marked in them");
    for (name, barrier, birth) in [
        ("insertion, white", Barrier::Insertion, Birth::White),
        ("insertion, black", Barrier::Insertion, Birth::Black),
        ("deletion", Barrier::Deletion, Birth::Black),
    ] {
        for units in [1, 2, 4, 8] {
            let heap = IncHeap::new(1 << 16, barrier, Pace::Alloc(units), MARK_STACK).born(birth);
            let way = checked(run(heap, list, true), list);
            println!(
                "  {name:17} {units:7} {:16} {:24} {:16} {:15}",
                way.counts.collections, way.steps.scans, way.counts.roots, way.counts.traced
            );
        }
    }
}
