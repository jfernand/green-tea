//! The generations test, `cargo xtask kernel --kernel gen`: green-tea-lisp's interpreter
//! on a heap with two generations, a nursery and an old space.
//!
//! First a few lines typed at the interpreter, with where one pair is after each. Then
//! seven programs, the five of the `lisp` and `gc` tests and two that store into old
//! objects: how many of their objects were copied out of the nursery, and what the write
//! barrier saw. Then one program with nurseries of growing size, a minor collection
//! beside a full one with 10,000 objects in the old space, a store that the barrier
//! wasn't told about, and a program run with a full collection at every allocation.
//!
//! Every number printed is a count, and is the same on every architecture and in debug
//! and release builds, except in the lines that say they are times.
//!
//! The longer tables are made on the host:
//! `cargo run --release -p green-tea-lisp --example generations`.

use alloc::format;
use alloc::string::String;
use green_tea_bare::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::{Board, Platform, println};
use green_tea_lisp::gc::{Collect, Counts, reachable};
use green_tea_lisp::generational::{Barrier, GenHeap, Generations, MUTATING};
use green_tea_lisp::programs::{PROGRAMS, Program};
use green_tea_lisp::semispace::Semispace;
use green_tea_lisp::{Heap, Kind, Lisp, Obj, Stats, Val, Value};

/// The heap the programs are compared in, in words of 8 bytes: the one of the `gc` test.
const MEDIUM: usize = 4096;
/// The nursery the programs are compared with.
const NURSERY: usize = 512;

type Failure = String;

pub fn test(fb: Framebuffer) {
    match session()
        .and_then(|()| programs())
        .and_then(|totals| Ok((totals, sizes()?, old_space()?, missed_store()?, stress()?)))
    {
        Err(e) => println!("FAIL: {e}"),
        Ok(((allocated, promoted), ..)) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: seven programs on two generations: {promoted} of {allocated} objects left the nursery alive, and nothing was left"
            );
        }
    }
}

/// A few lines typed at the interpreter, and where the first pair of `keep` is after each.
fn session() -> Result<(), Failure> {
    let mut lisp = Lisp::new(GenHeap::new(MEDIUM, 64, Barrier::Filter));
    println!("a heap of {MEDIUM} words, of which the top 64 are the nursery; an object is 3 or 4 words");
    let lines = [
        "(define keep (cons 1 (cons 2 nil)))",
        "(define (count n) (if (= n 0) 0 (count (- n 1))))",
        "(count 20)",
        "(set-cdr! keep (cons 3 nil))",
        "(count 20)",
        "keep",
    ];
    let mut places = [(0, 0); 6];
    for (n, line) in lines.iter().enumerate() {
        let (stats, before) = (lisp.heap.stats(), lisp.heap.generations());
        let value = lisp.run(line).map_err(|e| format!("{line}: {e:?}"))?;
        let after = lisp.heap.generations();
        let keep = lisp.global("keep").and_then(Value::as_obj).ok_or("keep is not a pair")?;
        let rest = lisp.heap.field(keep, 1).as_obj().ok_or("keep has one element")?;
        let space = |obj: Obj| if lisp.heap.is_young(obj) { "in the nursery" } else { "in the old space" };
        println!("> {line}");
        println!("{value}");
        println!(
            "  allocated {:2}; minor collections {}, objects copied {:2}; stores recorded {}, remembered set {}",
            lisp.heap.stats().allocated - stats.allocated,
            after.minor - before.minor,
            after.promoted - before.promoted,
            after.recorded - before.recorded,
            lisp.heap.remembered()
        );
        println!(
            "  keep is at word {:4}, {}, and its second pair at word {:4}, {}",
            keep.0,
            space(keep),
            rest.0,
            space(rest)
        );
        places[n] = (usize::from(lisp.heap.is_young(keep)), usize::from(lisp.heap.is_young(rest)));
        if n == 5 && value != "(1 3)" {
            return Err(format!("keep is {value}"));
        }
    }
    // Young when made, old after the first call, a young second pair after the store, and
    // old again once a minor collection has gone by.
    if places[0] != (1, 1) || places[2] != (0, 0) || places[3] != (0, 1) || places[4] != (0, 0) {
        return Err(format!("keep was not where it should be: {places:?}"));
    }
    let generations = lisp.heap.generations();
    if (generations.stores, generations.old_to_young, generations.recorded) != (1, 1, 1) {
        return Err(format!("the barrier recorded {} stores", generations.recorded));
    }
    Ok(())
}

/// What a program did to a heap.
struct Outcome {
    /// As the program left the heap, before any collection the test asked for.
    stats: Stats,
    counts: Counts,
    generations: Generations,
}

/// A run that must give the program's value, pass a check of what `reachable` finds
/// after a collection, and leave nothing once the definitions are forgotten.
fn run<H: Collect>(
    heap: H,
    program: &Program,
    stress: bool,
    generations: fn(&H) -> Generations,
) -> Result<Outcome, Failure> {
    let mut lisp = Lisp::new(heap);
    lisp.heap.set_stress(stress);
    lisp.heap.set_clock(Board::ticks);
    let value = lisp.run(program.source);
    let outcome =
        Outcome { stats: lisp.heap.stats(), counts: lisp.heap.counts(), generations: generations(&lisp.heap) };
    lisp.heap.collect();
    let after = lisp.heap.stats();
    if reachable(&lisp.heap) != (after.live(), after.live_words) {
        return Err(format!("{}: a collection left objects that the root stack can't reach", program.name));
    }
    lisp.forget_globals();
    lisp.heap.collect();
    let left = lisp.heap.stats().live();
    if value.as_deref() != Ok(program.value) || left != 0 {
        return Err(format!("{} in {} words: {value:?}, and {left} objects left", program.name, lisp.heap.capacity()));
    }
    Ok(outcome)
}

fn two(words: usize, nursery: usize, barrier: Barrier, program: &Program, stress: bool) -> Result<Outcome, Failure> {
    run(GenHeap::new(words, nursery, barrier), program, stress, GenHeap::generations)
}

/// Seven programs in `MEDIUM` words: the collections of a heap that copies, and of two
/// generations, and then what the write barrier saw. Returns the objects allocated and
/// the objects promoted, over all seven.
fn programs() -> Result<(u64, u64), Failure> {
    println!("seven programs in {MEDIUM} words: a heap that copies, and two generations with a nursery of {NURSERY}:");
    println!(
        "  program   heap             allocated  collections  minor  copied  major  marked  most copied  most work at once"
    );
    let mut barriers = String::new();
    let mut totals = (0, 0);
    for program in PROGRAMS.iter().chain(&MUTATING) {
        let copying = run(Semispace::new(MEDIUM), program, false, |_| Generations::default())?;
        println!(
            "  {:8}  copying          {:9} {:12} {:>6} {:7} {:>6} {:>7} {:12} {:18}",
            program.name,
            copying.stats.allocated,
            copying.counts.collections,
            "",
            copying.counts.traced,
            "",
            "",
            copying.counts.largest_traced,
            copying.counts.largest_work
        );
        let outcome = two(MEDIUM, NURSERY, Barrier::Filter, program, false)?;
        let (stats, counts, g) = (outcome.stats, outcome.counts, outcome.generations);
        println!(
            "  {:8}  two generations  {:9} {:12} {:6} {:7} {:6} {:7} {:12} {:18}",
            program.name,
            stats.allocated,
            counts.collections,
            g.minor,
            g.promoted,
            g.major,
            g.marked,
            g.largest_minor,
            counts.largest_work
        );
        if stats.allocated != copying.stats.allocated || counts.traced != g.promoted + g.marked {
            return Err(format!(
                "{}: the two heaps saw {} and {} allocations",
                program.name, stats.allocated, copying.stats.allocated
            ));
        }
        let every = two(MEDIUM, NURSERY, Barrier::Every, program, false)?.generations;
        barriers.push_str(&format!(
            "  {:8} {:7} {:9} {:13} {:9} {:16} {:8} {:6} {:25}\n",
            program.name,
            g.stores,
            g.stores_old,
            g.old_to_young,
            g.recorded,
            g.remembered_most,
            g.remembered_scanned,
            g.remembered_found,
            every.recorded
        ));
        // The barrier that tests nothing records every store, and finds the same references.
        if every.recorded != g.stores || every.promoted != g.promoted || every.remembered_found != g.remembered_found {
            return Err(format!("{}: the two barriers disagree", program.name));
        }
        totals.0 += stats.allocated;
        totals.1 += g.promoted;
    }
    println!("what the write barrier saw in those runs:");
    println!(
        "  program   stores  into old  old to young  recorded  most remembered  scanned  found  recorded, testing nothing"
    );
    for line in barriers.lines() {
        println!("{line}");
    }
    Ok(totals)
}

/// One program with nurseries of growing size, and then the times.
fn sizes() -> Result<(), Failure> {
    let program = &PROGRAMS[2];
    println!("{} in {MEDIUM} words, by the size of the nursery:", program.name);
    println!(
        "  nursery  minor  copied  died in the nursery  major  marked  most copied  most marked and walked  root slots"
    );
    let mut times = String::new();
    for nursery in [64, 256, 1024, usize::MAX] {
        let outcome = two(MEDIUM, nursery, Barrier::Filter, program, false)?;
        let (stats, counts, g) = (outcome.stats, outcome.counts, outcome.generations);
        let name = if nursery == usize::MAX { String::from("half") } else { format!("{nursery}") };
        println!(
            "  {name:>7} {:6} {:7} {:20} {:6} {:7} {:12} {:23} {:11}",
            g.minor,
            g.promoted,
            stats.allocated - g.promoted,
            g.major,
            g.marked,
            g.largest_minor,
            counts.largest_work,
            counts.roots
        );
        let microseconds = |ticks: u64| ticks * 1000 / Board::ticks_per_ms();
        times.push_str(&format!(
            "  a time, not a count: nursery {name}: {} minor took {} us, the longest {} us; {} major took {} us, the longest {} us\n",
            g.minor,
            microseconds(g.minor_pause_total),
            microseconds(g.minor_pause_longest),
            g.major,
            microseconds(g.major_pause_total),
            microseconds(g.major_pause_longest)
        ));
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

/// A list of 10,000 pairs in the old space, and what a minor collection reads of it:
/// nothing, and then the one pair that was stored into.
fn old_space() -> Result<(), Failure> {
    const KEPT: i64 = 10_000;
    println!("a list of {KEPT} pairs that is kept, and 100 pairs of garbage made after it:");
    let mut heap = GenHeap::new(1 << 16, NURSERY, Barrier::Filter);
    list(&mut heap, KEPT)?;
    heap.collect();
    let report = |name: &str, before: (Counts, Generations), heap: &GenHeap| {
        let (counts, g) = (heap.counts(), heap.generations());
        println!(
            "  {name:42} {:2} root slots read, {:2} remembered scanned, {:5} traced, {:5} walked",
            counts.roots - before.0.roots,
            g.remembered_scanned - before.1.remembered_scanned,
            counts.traced - before.0.traced,
            counts.walked - before.0.walked
        );
        counts.traced - before.0.traced
    };
    list(&mut heap, 100)?;
    heap.pop();
    let before = (heap.counts(), heap.generations());
    heap.minor();
    let nothing = report("a minor collection:", before, &heap);

    // A new pair, stored in the first pair of the old list and held nowhere else.
    list(&mut heap, 100)?;
    heap.pop();
    heap.push(Value::int(7));
    heap.push(Value::NIL);
    heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
    let (first, new) = (heap.root(0).as_obj().ok_or("no list")?, heap.top());
    heap.set_field(first, 0, new);
    heap.pop();
    let before = (heap.counts(), heap.generations());
    heap.minor();
    let one = report("after a store into the list's first pair:", before, &heap);
    let stored = heap.root(0).as_obj().map(|first| heap.field(first, 0)).and_then(Value::as_obj).ok_or("lost")?;
    if heap.is_young(stored) || heap.field(stored, 0) != Value::int(7) {
        return Err(String::from("the pair stored in an old pair didn't survive a minor collection"));
    }

    list(&mut heap, 100)?;
    heap.pop();
    let before = (heap.counts(), heap.generations());
    heap.collect();
    let all = report("a minor and a major collection:", before, &heap);

    let mut copying = Semispace::new(1 << 17);
    list(&mut copying, KEPT)?;
    list(&mut copying, 100)?;
    copying.pop();
    let first = copying.root(0).as_obj().ok_or("no list")?;
    copying.set_field(first, 0, Value::int(KEPT));
    copying.collect();
    let counts = copying.counts();
    println!(
        "  {:42} {:2} root slots read, {:2} remembered scanned, {:5} traced, {:5} walked",
        "a collection of a heap that copies:", counts.roots, 0, counts.traced, counts.walked
    );
    if (nothing, one, all, counts.traced) != (0, 1, KEPT as u64 + 1, KEPT as u64) {
        return Err(format!("the collections traced {nothing}, {one}, {all} and {} objects", counts.traced));
    }
    Ok(())
}

/// A reference to a nursery object stored in an old object without the barrier. The
/// minor collection doesn't copy the nursery object, and the next pair allocated takes
/// its words.
fn missed_store() -> Result<(), Failure> {
    let pair = |heap: &mut GenHeap, a: i64, b: i64| {
        heap.push(Value::int(a));
        heap.push(Value::int(b));
        heap.alloc(Kind::Pair).map_err(|_| Failure::from("the heap is full"))
    };
    let number = |v: Value| if let Val::Int(n) = v.decode() { n } else { -1 };
    let mut read = [(0, 0); 2];
    for (n, barrier) in [true, false].into_iter().enumerate() {
        let mut heap = GenHeap::new(MEDIUM, NURSERY, Barrier::Filter);
        pair(&mut heap, 0, 0)?;
        heap.minor(); // the pair (0 . 0) is old now, and in root slot 0
        let old = heap.top().as_obj().ok_or("no pair")?;
        pair(&mut heap, 1, 2)?;
        if barrier {
            heap.set_field(old, 0, heap.top());
        } else {
            heap.store_without_barrier(old, 0, heap.top());
        }
        heap.pop(); // the old pair's field is now the only reference to (1 . 2)
        heap.minor();
        pair(&mut heap, 3, 4)?;
        let old = heap.root(0).as_obj().ok_or("no pair")?;
        let stored = heap.field(old, 0).as_obj().ok_or("no reference")?;
        read[n] = (number(heap.field(stored, 0)), number(heap.field(stored, 1)));
    }
    println!(
        "the pair (1 . 2), made in the nursery and stored in an old pair; after a minor collection and an allocation:"
    );
    println!("  stored by set_field, the field refers to the pair ({} . {})", read[0].0, read[0].1);
    println!("  stored without the barrier, it refers to the pair ({} . {})", read[1].0, read[1].1);
    if read != [(1, 2), (3, 4)] {
        return Err(String::from("a store without the barrier was seen by the minor collection"));
    }
    Ok(())
}

/// A program with a minor and a major collection at every allocation.
fn stress() -> Result<(), Failure> {
    let program = &PROGRAMS[1];
    let outcome = two(MEDIUM, NURSERY, Barrier::Filter, program, true)?;
    let g = outcome.generations;
    println!("a minor and a major collection at every allocation:");
    println!(
        "  {:8} {} allocations, {} minor collections copied {} objects, and {} major collections marked {}",
        program.name, outcome.stats.allocated, g.minor, g.promoted, g.major, g.marked
    );
    if g.minor != outcome.stats.allocated || g.major != g.minor {
        return Err(format!("{}: {} collections", program.name, g.minor));
    }
    Ok(())
}
