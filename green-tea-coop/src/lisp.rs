//! The Lisp test, `cargo xtask kernel --kernel lisp`: green-tea-lisp's interpreter on its
//! reference-counting heap, with everything the heap does counted.
//!
//! First a few definitions typed one at a time, with the count of one pair after each.
//! Then the five programs of green-tea-lisp: what each allocates and frees, and how many
//! times a count is raised or lowered for it. Then a list freed three ways: by recursion
//! (in a task, so that its stack is a task's), from a queue, and lazily. Then the two
//! programs that make cycles, in a heap too small for what they leak, with and without the
//! cycle collector, and what the cycle collector costs the programs that make none.
//!
//! Every number printed is a count, and is the same on every architecture and in debug
//! and release builds, but for one line: the bytes of stack the recursion used.

use alloc::format;
use alloc::string::String;
use green_tea_bare::Framebuffer;
use green_tea_bare::runtime::{run, spawn};
use green_tea_bare::screen::Screen;
use green_tea_bare::{Global, println, stack};
use green_tea_lisp::programs::{PROGRAMS, Program};
use green_tea_lisp::rc::{Counts, Cycles, Freeing, LAZY_STEP, RcHeap};
use green_tea_lisp::{Error, Heap, Kind, Lisp, Stats, Value};

/// The heap each interpreter gets, in words of 8 bytes: 512 KiB, from the kernel's heap.
const WORDS: usize = 1 << 16;
/// A heap too small for what the cyclic programs leak.
const SMALL: usize = 1024;
/// How long a list the recursive heap frees, in a task. See `LONG`.
const SHORT: i64 = 16;
/// How long a list the other two free.
const LONG: i64 = 10_000;
/// How many candidates the cycle collector remembers before it searches.
const EVERY: usize = 256;

type Failure = String;

pub fn test(fb: Framebuffer) {
    match session()
        .and_then(|()| overwrite())
        .and_then(|()| programs())
        .and_then(|totals| Ok((totals, lists()?, cycles()?)))
    {
        Err(e) => println!("FAIL: {e}"),
        Ok((totals, (), ())) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: five programs allocated {} objects and freed {}, with {} increments and {} decrements",
                totals
                    .0
                    .allocated,
                totals
                    .0
                    .freed,
                totals
                    .1
                    .increments(),
                totals
                    .1
                    .decrements()
            );
        }
    }
}

fn check(heap: &RcHeap) -> Result<(), Failure> {
    heap.audit()
        .map_err(String::from)
}

/// Definitions typed one at a time. After each: what the heap holds, and the count of the
/// pair that `(cons 1 ...)` made.
fn session() -> Result<(), Failure> {
    let mut lisp = Lisp::new(RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak));
    println!("a heap of {WORDS} words; an object is a header word and two or three fields");
    let mut pair = None;
    let lines = ["(define a (cons 1 (cons 2 nil)))", "(define b a)", "(define a nil)", "(define b nil)"];
    for (line, (live, count)) in lines
        .iter()
        .zip([(3, 1), (4, 2), (4, 1), (2, 0)])
    {
        let before = lisp
            .heap
            .stats();
        let value = lisp
            .run(line)
            .map_err(|e| format!("{line}: {e:?}"))?;
        let after = lisp
            .heap
            .stats();
        pair = pair.or(lisp
            .global("a")
            .and_then(Value::as_obj));
        println!("> {line}");
        println!("{value}");
        let pair = pair.ok_or("a isn't a pair")?;
        let (allocated, freed) = (after.allocated - before.allocated, after.freed - before.freed);
        if after.live() == live
            && count > 0
            && lisp
                .heap
                .count(pair)
                == count
        {
            println!("  {allocated:2} allocated, {freed:2} freed, {live} live; the count of the pair (1 2) is {count}");
        } else if after.live() == live && count == 0 {
            println!("  {allocated:2} allocated, {freed:2} freed, {live} live; the pair (1 2) is freed");
        } else {
            return Err(format!("after {line}, {} objects are live", after.live()));
        }
        check(&lisp.heap)?;
    }
    Ok(())
}

/// A root slot that holds a list is overwritten with the list's own `cdr`: the new value's
/// only reference is inside the old one. A heap that dropped the old value before it
/// counted the new one would free both.
fn overwrite() -> Result<(), Failure> {
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak);
    list(&mut heap, 3)?;
    let slot = heap.roots() - 1;
    let rest = heap.field(
        heap.top()
            .as_obj()
            .ok_or("no list")?,
        1,
    );
    heap.set_root(slot, rest);
    let (freed, live) = (
        heap.stats()
            .freed,
        heap.stats()
            .live(),
    );
    println!("a slot that held a list of 3 pairs, overwritten with the list's cdr: {freed} pair freed, {live} live");
    check(&heap)?;
    if (freed, live) != (1, 2) {
        return Err(format!("overwriting a slot with its list's cdr freed {freed} pairs"));
    }
    Ok(())
}

/// Run a program, forget its definitions, and return what the heap counted.
fn measure(
    program: &Program,
    words: usize,
    freeing: Freeing,
    cycles: Cycles,
) -> Result<(RcHeap, Result<String, Error>), Failure> {
    let mut lisp = Lisp::new(RcHeap::new(words, freeing, cycles));
    let value = lisp.run(program.source);
    lisp.forget_globals();
    lisp.heap
        .finish_freeing();
    if cycles != Cycles::Leak {
        lisp.heap
            .collect_cycles();
    }
    check(&lisp.heap)?;
    if lisp
        .heap
        .roots()
        != 1
    {
        return Err(format!(
            "{}: {} values left on the root stack",
            program.name,
            lisp.heap
                .roots()
        ));
    }
    Ok((lisp.heap, value))
}

/// The five programs on a heap that frees by recursion and has no cycle collector.
fn programs() -> Result<(Stats, Counts), Failure> {
    println!("five programs, each in a heap of its own, with its definitions forgotten at the end:");
    println!("  program    value  allocated     freed    left  peak words  words touched  increments  decrements");
    let mut total = (Stats::default(), Counts::default());
    let mut shares = String::new();
    for program in &PROGRAMS {
        let (heap, value) = measure(program, WORDS, Freeing::Recursive, Cycles::Leak)?;
        let (stats, counts) = (heap.stats(), heap.counts());
        let value = value.map_err(|e| format!("{}: {e:?}", program.name))?;
        println!(
            "  {:8} {value:>7} {:10} {:9} {:7} {:11} {:14} {:11} {:11}",
            program.name,
            stats.allocated,
            stats.freed,
            stats.live(),
            stats.peak_words,
            heap.touched(),
            counts.increments(),
            counts.decrements()
        );
        if value != program.value {
            return Err(format!("{}: {value}, and it should be {}", program.name, program.value));
        }
        if (stats.live() != 0) != program.cyclic || heap.reachable() != (0, stats.live()) {
            return Err(format!("{}: {} objects left", program.name, stats.live()));
        }
        // An object starts with a count of one, and every counted reference is dropped
        // once, unless it is in an object that leaked.
        if !program.cyclic && counts.increments() + stats.allocated != counts.decrements() {
            return Err(format!("{}: increments and frees don't add up to decrements", program.name));
        }
        // The other two ways of freeing must do the same work.
        for freeing in [Freeing::Queued, Freeing::Lazy] {
            let (other, _) = measure(program, WORDS, freeing, Cycles::Leak)?;
            let same = other
                .stats()
                .allocated
                == stats.allocated
                && other
                    .stats()
                    .freed
                    == stats.freed
                && other
                    .counts()
                    .increments()
                    == counts.increments()
                && other
                    .counts()
                    .decrements()
                    == counts.decrements();
            if !same {
                return Err(format!("{}: {freeing:?} counted differently", program.name));
            }
        }
        let operations = counts.increments() + counts.decrements();
        let roots = counts.root_increments + counts.root_decrements;
        shares.push_str(&format!(
            "  {:8} {:10} {:10} {:10} {:10} {:9}% {:13}\n",
            program.name,
            counts.root_increments,
            counts.root_decrements,
            counts.field_increments,
            counts.field_decrements,
            roots * 100 / operations,
            operations / stats.allocated
        ));
        total
            .0
            .allocated += stats.allocated;
        total
            .0
            .freed += stats.freed;
        total
            .1
            .root_increments += counts.root_increments;
        total
            .1
            .root_decrements += counts.root_decrements;
        total
            .1
            .field_increments += counts.field_increments;
        total
            .1
            .field_decrements += counts.field_decrements;
    }
    println!("where the counting is done: for a root slot, or for a field of an object:");
    println!("  program   root inc   root dec  field inc  field dec  for roots  per allocation");
    for line in shares.lines() {
        println!("{line}");
    }
    Ok(total)
}

/// Push a list of `n` integers, made straight on the heap.
fn list(heap: &mut RcHeap, n: i64) -> Result<(), Failure> {
    heap.push(Value::NIL);
    for i in 0..n {
        heap.push(Value::int(i));
        heap.push(heap.root(heap.roots() - 2));
        heap.alloc(Kind::Pair)
            .map_err(|_| "the heap is full")?;
        heap.set_root(heap.roots() - 2, heap.top());
        heap.pop();
    }
    Ok(())
}

/// What freeing a list recursively did, in a task: (the depth, the bytes of stack).
static RECURSION: Global<(usize, usize)> = Global::new((0, 0));

/// One list, dropped by one `pop`, under each way of freeing.
fn lists() -> Result<(), Failure> {
    println!("a list dropped by one pop:");
    // By recursion, on a task's stack, which has a guard page under it where the board
    // has paging. A list of `LONG` would run into it.
    run(
        || {
            spawn(|| {
                let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak);
                if list(&mut heap, SHORT).is_ok() {
                    heap.pop();
                    RECURSION.set((
                        heap.counts()
                            .deepest,
                        heap.counts()
                            .deepest_bytes,
                    ));
                }
            })
        },
        |_| {},
    );
    let (deepest, bytes) = RECURSION.get();
    println!("  recursive: {SHORT} pairs, freed by a recursion {deepest} deep");
    if deepest != SHORT as usize || bytes == 0 {
        return Err(format!("freeing {SHORT} pairs recursed {deepest} deep"));
    }
    let per_pair = bytes / (deepest - 1);
    println!(
        "    stack: {bytes} bytes, {per_pair} for each pair; a task's {} would hold {} pairs",
        stack::STACK_SIZE,
        stack::STACK_SIZE / per_pair
    );
    for (freeing, name) in [(Freeing::Queued, "queued"), (Freeing::Lazy, "lazy")] {
        let mut heap = RcHeap::new(WORDS, freeing, Cycles::Leak);
        list(&mut heap, LONG)?;
        heap.pop();
        let (freed, burst) = (
            heap.stats()
                .freed,
            heap.counts()
                .largest_burst,
        );
        println!(
            "  {name}: {LONG} pairs; the pop freed {freed}, and the recursion was {} deep",
            heap.counts()
                .deepest
        );
        let expected = if freeing == Freeing::Lazy { (0, 0) } else { (LONG as u64, LONG as u64) };
        if (freed, burst) != expected
            || heap
                .counts()
                .deepest
                != 0
        {
            return Err(format!("a {name} heap freed {freed} pairs at the pop"));
        }
        if freeing == Freeing::Lazy {
            // The pairs are freed as later allocations come: here, of another list as long.
            list(&mut heap, LONG)?;
            let stats = heap.stats();
            println!(
                "    {LONG} allocations later: {} freed, at most {} at one allocation; {} words touched",
                stats.freed,
                heap.counts()
                    .largest_burst,
                heap.touched()
            );
            if stats.freed != LONG as u64
                || heap
                    .counts()
                    .largest_burst
                    != LAZY_STEP as u64
            {
                return Err(format!("a lazy heap freed {} pairs in {LONG} allocations", stats.freed));
            }
            heap.pop();
            heap.finish_freeing();
        }
        check(&heap)?;
    }
    Ok(())
}

/// The cyclic programs where their leak doesn't fit, and the cycle collector everywhere.
fn cycles() -> Result<(), Failure> {
    println!("the programs that make cycles, in a heap of {SMALL} words:");
    for program in PROGRAMS
        .iter()
        .filter(|p| p.cyclic)
    {
        let (heap, value) = measure(program, SMALL, Freeing::Recursive, Cycles::Leak)?;
        let stats = heap.stats();
        println!(
            "  {:8} counts alone:    {value:?} after {} allocations, {} freed",
            program.name, stats.allocated, stats.freed
        );
        if value != Err(Error::HeapFull) {
            return Err(format!("{} fitted in {SMALL} words without a cycle collector", program.name));
        }
        let (heap, value) = measure(program, SMALL, Freeing::Recursive, Cycles::Collect { every: usize::MAX })?;
        let (stats, counts) = (heap.stats(), heap.counts());
        println!(
            "  {:8} trial deletion:  {value:?} after {} allocations, {} freed; {} searches freed {}",
            program.name, stats.allocated, stats.freed, counts.searches, counts.cycle_freed
        );
        if value.as_deref() != Ok(program.value) {
            return Err(format!("{} didn't finish with a cycle collector", program.name));
        }
    }
    println!("trial deletion with a search every {EVERY} candidates, in a heap of {WORDS} words:");
    println!("  program   candidates  searches  trial decrements  decrements  freed by a search    left");
    for program in &PROGRAMS {
        let (heap, value) = measure(program, WORDS, Freeing::Recursive, Cycles::Collect { every: EVERY })?;
        let (stats, counts) = (heap.stats(), heap.counts());
        println!(
            "  {:8} {:11} {:9} {:17} {:11} {:18} {:7}",
            program.name,
            counts.candidates,
            counts.searches,
            counts.trial_decrements,
            counts.decrements(),
            counts.cycle_freed,
            stats.live()
        );
        if value.as_deref() != Ok(program.value) || stats.live() != 0 || (counts.cycle_freed != 0) != program.cyclic {
            return Err(format!("{}: {value:?}, and {} objects left after a search", program.name, stats.live()));
        }
    }
    Ok(())
}
