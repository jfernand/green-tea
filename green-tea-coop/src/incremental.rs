//! The incremental test, `cargo xtask kernel --kernel incremental`: green-tea-lisp's
//! interpreter on a heap that marks and sweeps a few objects at a time, between the
//! steps of the program.
//!
//! First three objects and two stores, with a cycle half done: without a write barrier
//! the cycle frees an object that the program can reach, and with either of two barriers
//! it doesn't. Then a list reversed in place while a cycle marks it. Then two programs
//! with the cycle done whole and done in steps of a few units at each allocation: how
//! long the longest step is, and what the steps cost in objects kept that were garbage.
//! Every number printed up to there is a count, and is the same on every architecture
//! and in debug and release builds.
//!
//! Then the timer. A timer interrupt every `PERIOD_US` microseconds sets a flag, and
//! `alloc` does a step when it finds the flag set. Those lines say they are times: they
//! differ from run to run, and so do the counts on them, because how many ticks arrive
//! while a program runs depends on how fast the machine runs it.
//!
//! The larger tables are made on the host: `cargo run --release -p green-tea-lisp
//! --example incremental`.

use alloc::format;
use alloc::string::String;
use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::{Board, Global, Platform, TimerInterrupt, println};
use green_tea_lisp::gc::{Collect, Counts, reachable};
use green_tea_lisp::incremental::{Barrier, Birth, Colour, IncHeap, MUTATORS, PAUSE_BUCKETS, Pace, Phase, Steps};
use green_tea_lisp::programs::{PROGRAMS, Program};
use green_tea_lisp::{Heap, Kind, Lisp, Obj, Val, Value};

/// The heap the programs run in, in words of 8 bytes, and its mark stack.
const WORDS: usize = 4096;
const MARK_STACK: usize = 1024;
/// A heap with room to spare, for the parts that drive a cycle by hand.
const ROOMY: usize = 1 << 12;
/// The units of work at each allocation, where the pace is by allocation.
const UNITS: usize = 4;
/// The time between two ticks of the timer, the units of work at a tick where the pace
/// is by tick, and the length of a step where the pace is a slice of time.
const PERIOD_US: u64 = 200;
const TICK_UNITS: usize = 64;
const SLICE_US: u64 = 50;

type Failure = String;

pub fn test(fb: Framebuffer) {
    match hidden().and_then(|()| reversed()).and_then(|()| programs()).and_then(|steps| Ok((steps, timer()?))) {
        Err(e) => println!("FAIL: {e}"),
        Ok(((cycles, steps, largest), ticks)) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: {cycles} cycles in {steps} steps of at most {largest} units lost nothing; then {ticks} ticks of the timer"
            );
        }
    }
}

fn name(barrier: Barrier) -> &'static str {
    match barrier {
        Barrier::None => "no barrier",
        Barrier::Insertion => "insertion",
        Barrier::Deletion => "deletion",
    }
}

fn letter(colour: Colour) -> &'static str {
    match colour {
        Colour::White => "white",
        Colour::Grey => "grey",
        Colour::Black => "black",
    }
}

fn pair(heap: &mut IncHeap, car: Value, cdr: Value) -> Result<Obj, Failure> {
    heap.push(car);
    heap.push(cdr);
    heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
    heap.top().as_obj().ok_or_else(|| String::from("no pair"))
}

/// Three pairs. B holds the only reference to C. A cycle starts and makes A black; then
/// the program moves the reference to C from B to A, and the cycle goes on.
fn hidden() -> Result<(), Failure> {
    println!("three pairs: B refers to C, which is (1 . 2), and A to nothing; a cycle starts, and takes one step");
    for barrier in [Barrier::None, Barrier::Insertion, Barrier::Deletion] {
        let mut heap = IncHeap::new(ROOMY, barrier, Pace::Manual, MARK_STACK);
        heap.set_audit(true);
        let c = pair(&mut heap, Value::int(1), Value::int(2))?;
        heap.push(Value::NIL);
        heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?; // B = (C . nil), in C's slot
        let b = heap.top().as_obj().ok_or("no pair")?;
        let a = pair(&mut heap, Value::NIL, Value::NIL)?;
        let colours = |heap: &IncHeap| {
            format!("A {}, B {}, C {}", letter(heap.colour(a)), letter(heap.colour(b)), letter(heap.colour(c)))
        };
        heap.start();
        heap.step(1);
        println!("  {}:", name(barrier));
        println!("    after the step:                {}", colours(&heap));
        let reference = heap.field(b, 0);
        heap.set_field(a, 0, reference);
        println!("    the program stores C in A:     {}", colours(&heap));
        heap.set_field(b, 0, Value::NIL);
        println!("    and overwrites it in B:        {}", colours(&heap));
        while heap.phase() == Phase::Mark {
            heap.step(1);
        }
        let kept = heap.is_allocated(c);
        while heap.step(1) {}
        let steps = heap.steps();
        if !kept {
            // A still refers to it. The next pair allocated takes its words.
            pair(&mut heap, Value::int(3), Value::int(4))?;
            let through = heap.field(a, 0).as_obj().ok_or("A lost its reference")?;
            let number = |v: Value| if let Val::Int(n) = v.decode() { n } else { -1 };
            println!(
                "    the cycle ends:                C is freed; one more pair is allocated, and A refers to ({} . {})",
                number(heap.field(through, 0)),
                number(heap.field(through, 1))
            );
        } else {
            println!(
                "    the cycle ends:                C is kept; the barrier ran {} times and shaded {} object",
                steps.barriers, steps.shaded
            );
        }
        if kept != (barrier != Barrier::None) || steps.lost != u64::from(!kept) {
            return Err(format!("with {}, C was {}", name(barrier), if kept { "kept" } else { "freed" }));
        }
    }
    Ok(())
}

/// A list of 200 pairs reversed in place while a cycle marks it, two pairs turned round
/// for each unit of marking. Without a barrier the program stops when marking ends.
fn reversed() -> Result<(), Failure> {
    println!("a list of 200 pairs reversed in place during marking, two pairs to a unit of marking:");
    for barrier in [Barrier::None, Barrier::Insertion, Barrier::Deletion] {
        let mut heap = IncHeap::new(ROOMY, barrier, Pace::Manual, MARK_STACK);
        heap.set_audit(true);
        heap.push(Value::NIL);
        heap.push(Value::NIL);
        for i in 0..200 {
            heap.push(Value::int(i));
            heap.push(heap.root(1));
            heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
            heap.set_root(1, heap.top());
            heap.pop();
        }
        heap.start();
        let mut turned = 0;
        while let Some(pair) = heap.root(1).as_obj() {
            if barrier == Barrier::None && heap.phase() != Phase::Mark {
                break;
            }
            let rest = heap.field(pair, 1);
            heap.push(rest);
            heap.set_field(pair, 1, heap.root(0));
            heap.set_root(0, heap.root(1));
            heap.set_root(1, heap.top());
            heap.pop();
            turned += 1;
            if turned % 2 == 0 {
                heap.step(1);
            }
        }
        while heap.step(1) {}
        let steps = heap.steps();
        println!(
            "  {:10}  {turned:3} pairs turned; the barrier shaded {:3} objects; {} scans of the root stack; {:3} pairs left, {:3} freed in use",
            name(barrier),
            steps.shaded,
            steps.scans,
            heap.stats().live(),
            steps.lost
        );
        let expected = if barrier == Barrier::None { 199 } else { 0 };
        if steps.lost != expected || heap.stats().live() != 200 - expected {
            return Err(format!("with {}, {} pairs of the list were freed", name(barrier), steps.lost));
        }
    }
    Ok(())
}

/// The ways of running a cycle that the test compares.
#[derive(Clone, Copy)]
struct Way {
    name: &'static str,
    barrier: Barrier,
    birth: Birth,
    pace: Pace,
}

const WAYS: [Way; 4] = [
    Way { name: "whole", barrier: Barrier::Deletion, birth: Birth::Black, pace: Pace::Whole },
    Way { name: "insertion, white", barrier: Barrier::Insertion, birth: Birth::White, pace: Pace::Alloc(UNITS) },
    Way { name: "insertion, black", barrier: Barrier::Insertion, birth: Birth::Black, pace: Pace::Alloc(UNITS) },
    Way { name: "deletion", barrier: Barrier::Deletion, birth: Birth::Black, pace: Pace::Alloc(UNITS) },
];

/// Run a program, and check that it computed its value, that no cycle lost an object
/// (with `audit`, which makes each cycle check itself), and that nothing is left once
/// the interpreter forgets its definitions. Returns what the heap counted for the
/// program alone, before the two collections that this function asks for.
fn run(mut heap: IncHeap, program: &Program, what: &str, audit: bool) -> Result<(Steps, Counts), Failure> {
    heap.set_audit(audit);
    let mut lisp = Lisp::new(heap);
    let value = lisp.run(program.source);
    let (steps, counts) = (lisp.heap.steps(), lisp.heap.counts());
    if value.as_deref() != Ok(program.value) || steps.lost != 0 {
        return Err(format!("{} with {what}: {value:?}, and {} objects freed in use", program.name, steps.lost));
    }
    lisp.heap.collect();
    let after = lisp.heap.stats();
    if reachable(&lisp.heap) != (after.live(), after.live_words) {
        return Err(format!("{} with {what}: a collection left objects that the root stack can't reach", program.name));
    }
    lisp.forget_globals();
    lisp.heap.collect();
    if lisp.heap.stats().live() != 0 {
        return Err(format!("{} with {what}: {} objects left", program.name, lisp.heap.stats().live()));
    }
    Ok((steps, counts))
}

/// Two programs, one that only allocates and one that writes, each way.
fn programs() -> Result<(u64, u64, u64), Failure> {
    println!("two programs in {WORDS} words; in steps, a cycle does {UNITS} units of work at each allocation:");
    println!(
        "  program   cycle             cycles  steps  most units in a step  stalls  marked  swept  born marked  kept as garbage  stores  barrier ran  shaded"
    );
    let mut totals = (0, 0, 0);
    for program in [&PROGRAMS[0], &MUTATORS[0]] {
        for way in WAYS {
            let heap = IncHeap::new(WORDS, way.barrier, way.pace, MARK_STACK).born(way.birth);
            let (steps, counts) = run(heap, program, way.name, true)?;
            println!(
                "  {:8}  {:16}  {:6} {:6} {:21} {:7} {:7} {:6} {:12} {:16} {:7} {:12} {:7}",
                program.name,
                way.name,
                counts.collections,
                steps.steps,
                steps.largest,
                steps.stalls,
                counts.traced,
                counts.walked,
                steps.born_marked,
                steps.floating,
                steps.stores,
                steps.barriers,
                steps.shaded
            );
            // Only allocating white may fall so far behind the program that `alloc` runs out.
            if steps.stalls != 0 && way.birth == Birth::Black {
                return Err(format!("{}, {}: {} stalls", program.name, way.name, steps.stalls));
            }
            if !matches!(way.pace, Pace::Whole) {
                totals.0 += counts.collections;
                totals.1 += steps.steps;
                totals.2 = steps.largest.max(totals.2);
            }
        }
    }
    Ok(totals)
}

// ---- the timer -----------------------------------------------------------------------

/// A tick has happened that no allocation has seen yet, and when it happened.
static PENDING: Global<bool> = Global::new(false);
static TICKED_AT: Global<u64> = Global::new(0);
/// Ticks taken, ticks that an allocation found, and how long those waited to be found,
/// in the platform's ticks: the total and the longest.
static TICKS: Global<u64> = Global::new(0);
static FOUND: Global<u64> = Global::new(0);
static WAITED: Global<u64> = Global::new(0);
static WAITED_LONGEST: Global<u64> = Global::new(0);

/// The interrupt handler: note the tick, and ask for the next. It touches nothing of the
/// heap, which the interrupted code may be half way through changing.
fn on_tick() {
    TICKS.set(TICKS.get() + 1);
    let now = Board::ticks();
    if !PENDING.get() {
        PENDING.set(true);
        TICKED_AT.set(now);
    }
    Board::set_deadline(now + PERIOD_US * Board::ticks_per_ms() / 1000);
}

/// What `alloc` asks: has a tick happened since it last asked?
fn ticker() -> bool {
    Board::without_interrupts(|| {
        if !PENDING.get() {
            return false;
        }
        PENDING.set(false);
        let waited = Board::ticks() - TICKED_AT.get();
        FOUND.set(FOUND.get() + 1);
        WAITED.set(WAITED.get() + waited);
        WAITED_LONGEST.set(WAITED_LONGEST.get().max(waited));
        true
    })
}

fn microseconds() -> u64 {
    Board::ticks() * 1000 / Board::ticks_per_ms()
}

/// The programs again with every step timed, and with the pace set by the timer.
fn timer() -> Result<u64, Failure> {
    let ways = [
        Way { name: "whole", barrier: Barrier::Deletion, birth: Birth::Black, pace: Pace::Whole },
        Way {
            name: "4 units an allocation",
            barrier: Barrier::Deletion,
            birth: Birth::Black,
            pace: Pace::Alloc(UNITS),
        },
        Way { name: "64 units a tick", barrier: Barrier::Deletion, birth: Birth::Black, pace: Pace::Tick(TICK_UNITS) },
        Way { name: "50 us a tick", barrier: Barrier::Deletion, birth: Birth::Black, pace: Pace::Slice(SLICE_US) },
    ];
    println!(
        "a timer interrupt every {PERIOD_US} us sets a flag that alloc polls; nreverse in {WORDS} words, deletion barrier:"
    );
    Board::init_timer(on_tick);
    let mut ticks = 0;
    for way in ways {
        let program = &MUTATORS[0];
        let mut heap = IncHeap::new(WORDS, way.barrier, way.pace, MARK_STACK).born(way.birth);
        heap.set_clock(microseconds);
        heap.set_ticker(ticker);
        for global in [&TICKS, &FOUND, &WAITED, &WAITED_LONGEST] {
            global.set(0);
        }
        PENDING.set(false);
        let timed = matches!(way.pace, Pace::Tick(_) | Pace::Slice(_));
        let started = microseconds();
        if timed {
            Board::set_deadline(Board::ticks() + PERIOD_US * Board::ticks_per_ms() / 1000);
            Board::interrupts_on();
        }
        // No audit here: its walk of the heap would be timed as part of a step.
        let outcome = run(heap, program, way.name, false);
        Board::interrupts_off();
        Board::set_deadline(u64::MAX);
        let elapsed = microseconds() - started;
        let (steps, counts) = outcome?;
        // The buckets: steps of under 1 us, of 1, of 2 or 3, of 4 to 7, and so on.
        let mut pauses = String::new();
        let last = (0..PAUSE_BUCKETS).rev().find(|&b| steps.pauses[b] != 0).unwrap_or(0);
        for bucket in 0..=last {
            pauses.push_str(&format!(" {}", steps.pauses[bucket]));
        }
        println!(
            "  a time, not a count: {:21}  {:3} cycles, {:5} steps of at most {:4} units, {:2} stalls of at most {:4} units",
            way.name, counts.collections, steps.steps, steps.largest, steps.stalls, steps.largest_stall
        );
        println!(
            "  a time, not a count:   the run took {elapsed} us, the steps {} us, the longest {} us; steps by length, <1 us, 1, 2-3, 4-7, ...:{pauses}",
            counts.pause_total, counts.pause_longest
        );
        if timed {
            let found = FOUND.get();
            let per_ms = Board::ticks_per_ms();
            println!(
                "  a time, not a count:   {} ticks, {found} found by an allocation, after {} us on average and {} us at most",
                TICKS.get(),
                WAITED.get() * 1000 / per_ms / found.max(1),
                WAITED_LONGEST.get() * 1000 / per_ms
            );
            if found == 0 {
                return Err(format!("{}: no tick of the timer was seen", way.name));
            }
            ticks += TICKS.get();
        }
    }
    Ok(ticks)
}
