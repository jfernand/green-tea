//! The tracing test, `cargo xtask kernel --kernel gc`: green-tea-lisp's interpreter on
//! three heaps that trace: one that marks and sweeps, one that marks and compacts, and one
//! that copies.
//!
//! First the definitions that the `lisp` test types at a heap that counts, typed at one
//! that marks, and then a collection. Then the two programs that leak under reference
//! counting, in a heap too small for what they leaked. Then the five programs on each
//! heap: how often it collected, and how many objects it traced, walked and moved for it.
//! Then one program in heaps of growing size, where objects are after a collection, the
//! mark stack, a reference that the heap wasn't told about, and a program run with a
//! collection at every allocation.
//!
//! Every number printed is a count, and is the same on every architecture and in debug
//! and release builds, except in the lines that say they are times.
//!
//! The smallest heap each program runs in is found on the host, where a thousand runs
//! cost nothing: `cargo run --release -p green-tea-lisp --example heaps`.

use alloc::format;
use alloc::string::String;
use green_tea_bare::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::{Board, Platform, println};
use green_tea_lisp::gc::{Collect, Counts, reachable};
use green_tea_lisp::mark::{MarkHeap, Reclaim};
use green_tea_lisp::programs::{PROGRAMS, Program};
use green_tea_lisp::semispace::Semispace;
use green_tea_lisp::{Error, Heap, Kind, Lisp, Obj, Stats, Val, Value};

/// A heap with room to spare, in words of 8 bytes.
const WORDS: usize = 1 << 16;
/// The heap the cyclic programs didn't fit in when their cycles were never freed.
const SMALL: usize = 1024;
/// The heap the five programs are compared in.
const MEDIUM: usize = 4096;
/// The entries of a mark stack.
const MARK_STACK: usize = 1024;

type Failure = String;

#[derive(Clone, Copy, PartialEq)]
enum Which {
    Sweep,
    Compact,
    Copy,
}

const HEAPS: [(Which, &str); 3] =
    [(Which::Sweep, "mark-sweep"), (Which::Compact, "mark-compact"), (Which::Copy, "copying")];

pub fn test(fb: Framebuffer) {
    match session()
        .and_then(|()| cycles())
        .and_then(|()| programs())
        .and_then(|totals| Ok((totals, sizes()?, moving()?, mark_stack()?, missed_root()?, stress()?)))
    {
        Err(e) => println!("FAIL: {e}"),
        Ok(((collections, traced), ..)) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: five programs on three heaps that trace: {collections} collections traced {traced} objects, and nothing was left"
            );
        }
    }
}

/// What a program did to a heap.
struct Outcome {
    value: Result<String, Error>,
    /// As the program left the heap, before any collection the test asked for.
    stats: Stats,
    counts: Counts,
    /// Objects still allocated after the definitions are forgotten and the heap collects.
    left: u64,
}

fn run<H: Collect>(heap: H, program: &Program, stress: bool, clock: bool) -> Result<Outcome, Failure> {
    let mut lisp = Lisp::new(heap);
    lisp.heap.set_stress(stress);
    if clock {
        lisp.heap.set_clock(Board::ticks);
    }
    let value = lisp.run(program.source);
    let (stats, counts) = (lisp.heap.stats(), lisp.heap.counts());
    lisp.heap.collect();
    // After a collection, what is allocated is what the root stack reaches.
    let after = lisp.heap.stats();
    if reachable(&lisp.heap) != (after.live(), after.live_words) {
        return Err(format!("{}: a collection left objects that the root stack can't reach", program.name));
    }
    lisp.forget_globals();
    lisp.heap.collect();
    Ok(Outcome { value, stats, counts, left: lisp.heap.stats().live() })
}

fn measure(which: Which, words: usize, program: &Program, stress: bool, clock: bool) -> Result<Outcome, Failure> {
    match which {
        Which::Sweep => run(MarkHeap::new(words, Reclaim::Sweep, MARK_STACK), program, stress, clock),
        Which::Compact => run(MarkHeap::new(words, Reclaim::Compact, MARK_STACK), program, stress, clock),
        Which::Copy => run(Semispace::new(words), program, stress, clock),
    }
}

/// A run that must give the program's value and leave nothing.
fn complete(which: Which, name: &str, words: usize, program: &Program, stress: bool) -> Result<Outcome, Failure> {
    let outcome = measure(which, words, program, stress, false)?;
    if outcome.value.as_deref() != Ok(program.value) || outcome.left != 0 {
        return Err(format!(
            "{} on a {name} heap of {words} words: {:?}, and {} objects left",
            program.name, outcome.value, outcome.left
        ));
    }
    Ok(outcome)
}

/// The definitions of the `lisp` test, typed at a heap that marks and sweeps.
fn session() -> Result<(), Failure> {
    let mut lisp = Lisp::new(MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK));
    println!("a heap of {WORDS} words; an object is a header word and two or three fields");
    let lines = ["(define a (cons 1 (cons 2 nil)))", "(define b a)", "(define a nil)", "(define b nil)"];
    for (line, reaches) in lines.iter().zip([3, 4, 4, 2]) {
        let before = lisp.heap.stats();
        let value = lisp.run(line).map_err(|e| format!("{line}: {e:?}"))?;
        let after = lisp.heap.stats();
        let reached = reachable(&lisp.heap).0;
        println!("> {line}");
        println!("{value}");
        println!(
            "  {:2} allocated, {:2} freed; {:2} objects in the heap, and the root stack reaches {reached}",
            after.allocated - before.allocated,
            after.freed - before.freed,
            after.live()
        );
        if reached != reaches || after.freed != 0 {
            return Err(format!("after {line}, the root stack reaches {reached} objects"));
        }
    }
    let before = lisp.heap.stats();
    lisp.heap.collect();
    let (after, counts) = (lisp.heap.stats(), lisp.heap.counts());
    println!(
        "a collection: {} root slot read, {} objects marked, {} passed over by the sweep, {} freed; {} in the heap",
        counts.roots,
        counts.traced,
        counts.walked,
        after.freed - before.freed,
        after.live()
    );
    if (counts.traced, after.live(), counts.walked) != (2, 2, before.live()) {
        return Err(format!("a collection marked {} objects and left {}", counts.traced, after.live()));
    }
    Ok(())
}

/// The programs that leak under reference counting, where their leak didn't fit.
fn cycles() -> Result<(), Failure> {
    println!("the programs that make cycles, in a heap of {SMALL} words:");
    for program in PROGRAMS.iter().filter(|p| p.cyclic) {
        for (which, name) in HEAPS {
            let outcome = complete(which, name, SMALL, program, false)?;
            println!(
                "  {:8} {name:13} {:?} after {} allocations and {} collections; {} left",
                program.name, outcome.value, outcome.stats.allocated, outcome.counts.collections, outcome.left
            );
        }
    }
    Ok(())
}

/// The five programs on each heap, in a heap of `MEDIUM` words. Returns the collections
/// made and the objects traced, over all of them.
fn programs() -> Result<(u64, u64), Failure> {
    println!("five programs on each heap, in {MEDIUM} words, with a mark stack of {MARK_STACK} entries:");
    println!(
        "  program  heap          allocated  collections   traced  most at once   walked  words moved  mark stack"
    );
    let mut totals = (0, 0);
    for program in &PROGRAMS {
        let mut allocated = None;
        for (which, name) in HEAPS {
            let outcome = complete(which, name, MEDIUM, program, false)?;
            let (stats, counts) = (outcome.stats, outcome.counts);
            println!(
                "  {:8} {name:13} {:9} {:12} {:8} {:13} {:8} {:12} {:11}",
                program.name,
                stats.allocated,
                counts.collections,
                counts.traced,
                counts.largest_traced,
                counts.walked,
                counts.moved_words,
                counts.deepest
            );
            // The heap doesn't change what the program allocates.
            if *allocated.get_or_insert(stats.allocated) != stats.allocated {
                return Err(format!("{}: a {name} heap saw {} allocations", program.name, stats.allocated));
            }
            if counts.overflows != 0 || (which == Which::Copy) != (counts.walked == 0) {
                return Err(format!("{}: a {name} heap walked {} objects", program.name, counts.walked));
            }
            totals.0 += counts.collections;
            totals.1 += counts.traced;
        }
    }
    Ok(totals)
}

/// One program in heaps of growing size: what a collection costs, and how many there are.
fn sizes() -> Result<(), Failure> {
    let program = &PROGRAMS[0];
    println!("{} in heaps of growing size:", program.name);
    println!("   words  heap          collections   traced   walked  words moved  most traced and walked at once");
    let mut times = String::new();
    for words in [2048, 8192, 32768] {
        for (which, name) in HEAPS {
            let outcome = measure(which, words, program, false, true)?;
            let counts = outcome.counts;
            if outcome.value.as_deref() != Ok(program.value) {
                return Err(format!("{} on a {name} heap of {words} words: {:?}", program.name, outcome.value));
            }
            println!(
                "  {words:6}  {name:13} {:11} {:8} {:8} {:12} {:31}",
                counts.collections, counts.traced, counts.walked, counts.moved_words, counts.largest_work
            );
            let microseconds = |ticks: u64| ticks * 1000 / Board::ticks_per_ms();
            times.push_str(&format!(
                "  a time, not a count: {name} in {words} words, {} collections took {} us, the longest {} us\n",
                counts.collections,
                microseconds(counts.pause_total),
                microseconds(counts.pause_longest)
            ));
        }
    }
    // Times differ from run to run, and an emulator's more than most.
    for line in times.lines() {
        println!("{line}");
    }
    Ok(())
}

/// Push a list of the integers `n` down to 1, made straight on the heap.
fn list<H: Heap>(heap: &mut H, n: i64) -> Result<(), Failure> {
    heap.push(Value::NIL);
    for i in 1..=n {
        heap.push(Value::int(i));
        heap.push(heap.root(heap.roots() - 2));
        heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
        heap.set_root(heap.roots() - 2, heap.top());
        heap.pop();
    }
    Ok(())
}

/// Where the pairs of the list in the top root slot are.
fn places<H: Heap>(heap: &H) -> String {
    let (mut out, mut rest) = (String::new(), heap.top());
    while let Some(pair) = rest.as_obj() {
        out.push_str(&format!(" {}", pair.0));
        rest = heap.field(pair, 1);
    }
    out
}

/// A list of four pairs with garbage before it, and where each heap has it after a
/// collection, and then puts the next pair.
fn moving() -> Result<(), Failure> {
    fn show<H: Collect>(mut heap: H, name: &str) -> Result<String, Failure> {
        list(&mut heap, 4)?;
        heap.pop();
        list(&mut heap, 4)?;
        let before = places(&heap);
        heap.collect();
        let after = places(&heap);
        heap.push(Value::NIL);
        heap.push(Value::NIL);
        heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
        let next = heap.top().as_obj().map_or(0, |obj| obj.0);
        println!("  {name:13} before:{before}; after:{after}; the next pair allocated is at {next}");
        Ok(after)
    }
    println!("a list of 4 pairs, made after 4 pairs of garbage, in a heap of 64 words: where its pairs are");
    let sweep = show(MarkHeap::new(64, Reclaim::Sweep, MARK_STACK), "mark-sweep")?;
    let compact = show(MarkHeap::new(64, Reclaim::Compact, MARK_STACK), "mark-compact")?;
    let copy = show(Semispace::new(64), "copying")?;
    if (sweep.as_str(), compact.as_str(), copy.as_str()) != (" 22 19 16 13", " 10 7 4 1", " 32 35 38 41") {
        return Err(format!("after a collection the list is at{sweep},{compact} and{copy}"));
    }
    Ok(())
}

/// What the mark stack holds for two shapes of list, and what happens when it is too short.
fn mark_stack() -> Result<(), Failure> {
    println!("the mark stack:");
    let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK);
    list(&mut heap, 10_000)?;
    heap.collect();
    let counts = heap.counts();
    println!("  a list of 10000 integers: {} objects marked, at most {} on the stack", counts.traced, counts.deepest);
    if (counts.traced, counts.deepest) != (10_000, 1) {
        return Err(format!("a list of integers needed a mark stack of {}", counts.deepest));
    }
    for limit in [MARK_STACK, 16] {
        // A list of 500 lists of 3 integers.
        let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, limit);
        heap.push(Value::NIL);
        for _ in 0..500 {
            list(&mut heap, 3)?;
            heap.push(heap.root(0));
            heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
            heap.set_root(0, heap.top());
            heap.pop();
        }
        heap.collect();
        let counts = heap.counts();
        println!(
            "  a list of 500 lists, a stack of {limit:4}: {} objects marked, at most {} on the stack",
            counts.traced, counts.deepest
        );
        if counts.overflows != 0 {
            println!(
                "    {} objects didn't fit; the heap was searched {} times, and {} objects passed over",
                counts.overflows,
                counts.rescans,
                counts.walked - heap.stats().allocated
            );
        }
        if counts.traced != 2000 || heap.stats().live() != 2000 || (counts.overflows == 0) != (limit == MARK_STACK) {
            return Err(format!("with a mark stack of {limit}, {} objects were marked", counts.traced));
        }
    }
    Ok(())
}

/// A reference kept in a variable across a collection, where the heap can't see it. The
/// pair it names is freed, and the next pair allocated takes its words.
fn missed_root() -> Result<(), Failure> {
    let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK);
    heap.push(Value::int(1));
    heap.push(Value::int(2));
    heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
    let borrowed: Obj = heap.top().as_obj().ok_or("no pair")?;
    let before = (heap.field(borrowed, 0), heap.field(borrowed, 1));
    heap.pop(); // the variable is now the only reference
    heap.collect();
    heap.push(Value::int(3));
    heap.push(Value::int(4));
    heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
    let after = (heap.field(borrowed, 0), heap.field(borrowed, 1));
    let number = |v: Value| if let Val::Int(n) = v.decode() { n } else { -1 };
    println!(
        "a reference to the pair ({} . {}), kept in a variable across a collection and one allocation:",
        number(before.0),
        number(before.1)
    );
    println!("  now it is a reference to the pair ({} . {})", number(after.0), number(after.1));
    if after != (Value::int(3), Value::int(4)) {
        return Err(String::from("a pair that nothing referred to survived a collection"));
    }
    Ok(())
}

/// A program with a collection at every allocation.
fn stress() -> Result<(), Failure> {
    println!("a collection at every allocation:");
    for program in [&PROGRAMS[1]] {
        for (which, name) in HEAPS {
            let outcome = complete(which, name, MEDIUM, program, true)?;
            println!(
                "  {:8} {name:13} {:?} after {} allocations and {} collections, which traced {} objects",
                program.name, outcome.value, outcome.stats.allocated, outcome.counts.collections, outcome.counts.traced
            );
            if outcome.counts.collections != outcome.stats.allocated {
                return Err(format!("{}: {} collections", program.name, outcome.counts.collections));
            }
        }
    }
    Ok(())
}
