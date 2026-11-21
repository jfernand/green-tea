//! Tests of the incremental heap. The first group drives a cycle by hand, a step at a
//! time, around a program that changes the graph between the steps. The second runs the
//! tests that `gc_tests.rs` runs on the heaps of Chapter 13, on every combination of
//! barrier, birth colour and pace, with the audit on.

use super::*;
use crate::gc::reachable;
use crate::programs::PROGRAMS;
use crate::rc::{Cycles, Freeing, RcHeap};
use crate::{Error, Lisp};
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::string::String;
use std::vec::Vec;

const WORDS: usize = 1 << 16;
const MARK_STACK: usize = 1024;

fn manual(barrier: Barrier) -> IncHeap {
    let mut heap = IncHeap::new(WORDS, barrier, Pace::Manual, MARK_STACK);
    heap.set_audit(true);
    heap
}

/// Push a pair of two values.
fn pair(heap: &mut IncHeap, car: Value, cdr: Value) -> Obj {
    heap.push(car);
    heap.push(cdr);
    heap.alloc(Kind::Pair).unwrap();
    heap.top().as_obj().unwrap()
}

/// Push a list of the integers `n - 1` down to 0.
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

fn elements<H: Heap>(heap: &H, slot: usize) -> Vec<Value> {
    let (mut out, mut rest) = (Vec::new(), heap.root(slot));
    while let Some(pair) = rest.as_obj() {
        out.push(heap.field(pair, 0));
        rest = heap.field(pair, 1);
    }
    out
}

/// The scene of the chapter's opening: B, which holds the only reference to C, and A,
/// in root slots 0 and 1. A cycle starts and takes one step, which makes A black.
fn scene(barrier: Barrier) -> (IncHeap, Obj, Obj, Obj) {
    let mut heap = manual(barrier);
    let c = pair(&mut heap, Value::int(1), Value::int(2));
    heap.push(Value::NIL);
    heap.alloc(Kind::Pair).unwrap(); // B = (C . nil), in C's slot
    let b = heap.top().as_obj().unwrap();
    let a = pair(&mut heap, Value::NIL, Value::NIL);
    heap.start();
    heap.step(1);
    assert_eq!((heap.colour(a), heap.colour(b), heap.colour(c)), (Colour::Black, Colour::Grey, Colour::White));
    (heap, a, b, c)
}

/// Move the reference to C from B to A: the two stores of Dijkstra's counterexample.
fn hide(heap: &mut IncHeap, a: Obj, b: Obj) {
    let c = heap.field(b, 0);
    heap.set_field(a, 0, c);
    heap.set_field(b, 0, Value::NIL);
}

#[test]
fn without_a_barrier_a_reachable_object_is_freed() {
    let (mut heap, a, b, c) = scene(Barrier::None);
    hide(&mut heap, a, b);
    assert_eq!((heap.colour(a), heap.colour(c)), (Colour::Black, Colour::White)); // black refers to white
    while heap.step(1) {}
    assert_eq!(heap.steps().lost, 1);
    assert_eq!(heap.field(a, 0), Value::obj(c));
    assert!(!heap.is_allocated(c));
    // The next pair allocated takes its words, and A's car is now another pair.
    let other = pair(&mut heap, Value::int(3), Value::int(4));
    assert_eq!(other, c);
    assert_eq!(heap.field(c, 0), Value::int(3));
}

#[test]
fn the_insertion_barrier_shades_the_object_stored() {
    let (mut heap, a, b, c) = scene(Barrier::Insertion);
    hide(&mut heap, a, b);
    assert_eq!(heap.colour(c), Colour::Grey);
    assert_eq!((heap.steps().barriers, heap.steps().shaded), (2, 1));
    while heap.step(1) {}
    assert_eq!((heap.steps().lost, heap.steps().floating), (0, 0));
    assert!(heap.is_allocated(c));
    assert_eq!(heap.field(c, 1), Value::int(2));
}

#[test]
fn the_deletion_barrier_shades_the_object_overwritten() {
    let (mut heap, a, b, c) = scene(Barrier::Deletion);
    let reference = heap.field(b, 0);
    heap.set_field(a, 0, reference);
    assert_eq!(heap.colour(c), Colour::White); // a black object refers to a white one, for now
    heap.set_field(b, 0, Value::NIL);
    assert_eq!(heap.colour(c), Colour::Grey);
    assert_eq!((heap.steps().barriers, heap.steps().shaded), (2, 1));
    while heap.step(1) {}
    assert_eq!((heap.steps().lost, heap.steps().floating), (0, 0));
    assert!(heap.is_allocated(c));
}

/// The reference to C is overwritten and stored nowhere: C is garbage before the cycle
/// reaches it. The insertion barrier lets the cycle free it. The deletion barrier keeps
/// it for a cycle.
#[test]
fn what_dies_during_a_cycle_floats_under_the_deletion_barrier_only() {
    for (barrier, floating) in [(Barrier::Insertion, 0), (Barrier::Deletion, 1)] {
        let (mut heap, _, b, c) = scene(barrier);
        heap.set_field(b, 0, Value::NIL);
        while heap.step(1) {}
        assert_eq!((heap.steps().floating, heap.steps().lost), (floating, 0), "{barrier:?}");
        assert_eq!(heap.is_allocated(c), floating == 1);
        heap.collect();
        assert!(!heap.is_allocated(c));
        assert_eq!(heap.stats().live(), 2);
    }
}

/// A pair allocated during marking and dropped at once.
#[test]
fn an_object_born_white_and_dropped_is_freed_by_the_cycle_it_was_born_in() {
    for (barrier, birth, floating) in [
        (Barrier::Insertion, Birth::White, 0),
        (Barrier::Insertion, Birth::Black, 1),
        (Barrier::Deletion, Birth::White, 1), // the deletion barrier allocates black whatever it is told
    ] {
        let mut heap = manual(barrier).born(birth);
        list(&mut heap, 10);
        heap.start();
        heap.step(3);
        let fresh = pair(&mut heap, Value::int(1), Value::int(2));
        assert_eq!(heap.colour(fresh) == Colour::Black, floating == 1);
        heap.pop();
        while heap.step(1) {}
        assert_eq!((heap.steps().floating, heap.steps().lost), (floating, 0), "{barrier:?} {birth:?}");
        assert_eq!(heap.stats().live(), 10 + floating);
    }
}

/// A pair allocated white during marking and kept on the root stack, which no barrier
/// watches: the scan of the root stack at the end of marking is what finds it.
#[test]
fn an_object_born_white_and_kept_is_found_by_scanning_the_roots_again() {
    let mut heap = manual(Barrier::Insertion);
    list(&mut heap, 10);
    heap.start();
    heap.step(3);
    let fresh = pair(&mut heap, Value::int(1), Value::int(2));
    assert_eq!(heap.colour(fresh), Colour::White);
    while heap.step(1) {}
    assert_eq!((heap.steps().scans, heap.steps().rescans, heap.steps().lost), (3, 2, 0)); // one found it, one found nothing
    assert!(heap.is_allocated(fresh));
    assert_eq!(heap.stats().live(), 11);
}

/// A black newborn whose fields refer to a white object that is nowhere else: the
/// reference was read from a grey object, which then lost it, and was popped from the
/// root stack by the `alloc` that made the newborn.
#[test]
fn a_black_newborn_has_its_fields_shaded() {
    let (heap, _, b, c) = scene(Barrier::Insertion);
    let mut heap = heap.born(Birth::Black);
    heap.push(heap.field(b, 0));
    heap.set_field(b, 0, Value::NIL);
    assert_eq!(heap.colour(c), Colour::White);
    heap.push(Value::NIL);
    heap.alloc(Kind::Pair).unwrap(); // (C . nil), black
    assert_eq!(heap.colour(heap.top().as_obj().unwrap()), Colour::Black);
    assert_eq!(heap.colour(c), Colour::Grey);
    while heap.step(1) {}
    assert_eq!(heap.steps().lost, 0);
    assert!(heap.is_allocated(c));
}

/// A list of 200 pairs reversed in place, two pairs for each unit of marking: the program
/// turns round the very references that the mark phase is following, and gets ahead of
/// it. Slot 0 holds the part already reversed, slot 1 the part still to do. Without a
/// barrier the program stops when marking ends, before it touches what the sweep frees.
fn reverse_while_marking(barrier: Barrier) -> IncHeap {
    let mut heap = manual(barrier);
    heap.push(Value::NIL);
    list(&mut heap, 200);
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
    heap
}

#[test]
fn a_list_reversed_during_marking_is_kept_whole_by_either_barrier() {
    let heap = reverse_while_marking(Barrier::Deletion);
    // Every second store overwrote the reference to a pair that marking hadn't reached.
    assert_eq!((heap.steps().lost, heap.steps().barriers, heap.steps().shaded), (0, 200, 100));
    assert_eq!(elements(&heap, 0), (0..200).map(Value::int).collect::<Vec<_>>());

    let heap = reverse_while_marking(Barrier::Insertion);
    // Nearly every store put in a pair a reference to the pair before it, which only a
    // root slot had referred to since the cycle started, and which was white. The part
    // of the list still to do is kept by the root slot it is in, scanned again.
    assert_eq!((heap.steps().lost, heap.steps().barriers, heap.steps().shaded), (0, 200, 198));
    assert!(heap.steps().rescans >= 2, "{}", heap.steps().rescans);
    assert_eq!(elements(&heap, 0), (0..200).map(Value::int).collect::<Vec<_>>());
}

#[test]
fn a_list_reversed_during_marking_without_a_barrier_is_freed() {
    let heap = reverse_while_marking(Barrier::None);
    // Marking ended after one pair, which by then referred to nothing.
    assert_eq!(heap.steps().lost, 199);
    assert_eq!(heap.stats().live(), 1);
}

/// The sweep is in steps too. An object allocated in words that the sweep hasn't reached
/// is born marked, and survives it.
#[test]
fn an_object_allocated_ahead_of_the_sweep_survives_it() {
    let mut heap = manual(Barrier::Deletion);
    list(&mut heap, 6);
    heap.pop();
    heap.collect(); // six free pairs, the highest at the front of the list
    list(&mut heap, 2);
    heap.start();
    while heap.phase() == Phase::Mark {
        heap.step(1);
    }
    assert_eq!(heap.phase(), Phase::Sweep);
    let before = heap.steps().born_marked;
    list(&mut heap, 3); // from the free list, ahead of the sweep
    assert_eq!(heap.steps().born_marked - before, 3);
    while heap.step(1) {}
    assert_eq!(elements(&heap, 1), [2, 1, 0].map(Value::int));
    assert_eq!(elements(&heap, 0), [1, 0].map(Value::int));
    heap.collect();
    assert_eq!(heap.stats().live(), 5);
    assert_eq!(reachable(&heap), (5, 15));
}

#[test]
fn a_step_does_its_budget_and_a_scan_of_the_roots() {
    for (barrier, k) in [(Barrier::Insertion, 4), (Barrier::Deletion, 4), (Barrier::Deletion, 8)] {
        let mut lisp = Lisp::new(IncHeap::new(4096, barrier, Pace::Alloc(k), MARK_STACK));
        lisp.heap.set_audit(true);
        assert_eq!(lisp.run(PROGRAMS[0].source).unwrap(), PROGRAMS[0].value);
        let (steps, counts) = (lisp.heap.steps(), lisp.heap.counts());
        assert!(counts.collections >= 2, "{}", counts.collections);
        assert_eq!((steps.stalls, steps.lost), (0, 0));
        // The root stack of this program never holds more than 200 slots.
        assert!(steps.largest <= k as u64 + 2 * 200, "{barrier:?} {k}: {}", steps.largest);
        assert!(steps.largest < counts.largest_work / 4, "{} of {}", steps.largest, counts.largest_work);
    }
}

#[test]
fn a_cycle_in_one_step_is_a_collection_of_chapter_13() {
    let mut lisp = Lisp::new(IncHeap::new(4096, Barrier::Deletion, Pace::Whole, MARK_STACK));
    lisp.heap.set_audit(true);
    assert_eq!(lisp.run(PROGRAMS[0].source).unwrap(), PROGRAMS[0].value);
    let (steps, counts) = (lisp.heap.steps(), lisp.heap.counts());
    assert_eq!((steps.steps, steps.barriers, steps.floating, steps.born_marked), (counts.collections, 0, 0, 0));
    // A step is a whole cycle: the root slots it read, the objects it marked, the objects it swept.
    assert_eq!(steps.units, counts.roots + counts.traced + counts.walked);
    assert!(steps.largest >= counts.largest_work);
}

/// A mark stack of two entries, and steps of one unit: the search for the objects that
/// didn't fit is taken a unit at a time too, with the program running in between.
#[test]
fn the_search_after_an_overflow_is_done_in_steps() {
    for barrier in [Barrier::Insertion, Barrier::Deletion] {
        let mut heap = IncHeap::new(WORDS, barrier, Pace::Manual, 2);
        heap.set_audit(true);
        heap.push(Value::NIL);
        for _ in 0..50 {
            list(&mut heap, 3);
            heap.push(heap.root(0));
            heap.alloc(Kind::Pair).unwrap();
            heap.set_root(0, heap.top());
            heap.pop();
        }
        heap.start();
        let mut steps = 0;
        while heap.step(1) {
            steps += 1;
            if steps % 7 == 0 && heap.phase() == Phase::Mark {
                // The program, meanwhile: drop the first list of the fifty, and add a pair.
                let first = heap.root(0).as_obj().unwrap();
                heap.set_field(first, 0, Value::NIL);
                let rest = heap.root(0);
                pair(&mut heap, Value::int(steps), rest);
                heap.set_root(0, heap.top());
                heap.pop();
            }
        }
        assert!(heap.counts().overflows > 0 && heap.counts().rescans > 0);
        assert_eq!(heap.steps().lost, 0, "{barrier:?}");
        assert_eq!(heap.steps().largest, 1);
        heap.collect();
        let (objects, words) = reachable(&heap);
        assert_eq!((heap.stats().live(), heap.stats().live_words), (objects, words));
    }
}

#[test]
fn a_heap_too_small_for_its_pace_stalls_and_still_computes() {
    // 1,400 words is a little over what `list` needs with a sweep (Chapter 13: 1,278).
    let mut lisp = Lisp::new(IncHeap::new(1400, Barrier::Deletion, Pace::Alloc(1), MARK_STACK));
    lisp.heap.set_audit(true);
    assert_eq!(lisp.run(PROGRAMS[0].source).unwrap(), PROGRAMS[0].value);
    let steps = lisp.heap.steps();
    assert!(steps.stalls > 0);
    assert!(steps.largest_stall > steps.largest);
    assert_eq!(steps.lost, 0);
}

#[test]
fn a_full_heap_is_an_error_and_the_roots_are_kept() {
    for barrier in [Barrier::Insertion, Barrier::Deletion] {
        let mut heap = IncHeap::new(60, barrier, Pace::Alloc(2), MARK_STACK);
        heap.push(Value::NIL);
        let mut pairs = 0;
        loop {
            heap.push(Value::int(pairs));
            heap.push(heap.root(0));
            if heap.alloc(Kind::Pair) == Err(Full) {
                assert_eq!(heap.roots(), 3);
                break;
            }
            heap.set_root(0, heap.top());
            heap.pop();
            pairs += 1;
        }
        assert_eq!(pairs, 20);
        heap.pop();
        heap.pop();
        assert_eq!(elements(&heap, 0), (0..20).rev().map(Value::int).collect::<Vec<_>>());
    }
}

// ---- every combination ---------------------------------------------------------------

std::thread_local! {
    static CALLS: Cell<u64> = const { Cell::new(0) };
}

/// A tick at every twentieth allocation that asks.
fn every_twentieth() -> bool {
    CALLS.with(|calls| {
        calls.set(calls.get() + 1);
        calls.get().is_multiple_of(20)
    })
}

/// A clock that advances by one each time it is read.
fn counting_clock() -> u64 {
    CALLS.with(|calls| {
        calls.set(calls.get() + 1);
        calls.get()
    })
}

type Make = fn(usize) -> IncHeap;

fn audited(mut heap: IncHeap) -> IncHeap {
    heap.set_audit(true);
    heap
}

const HEAPS: [(&str, Make); 9] = [
    ("insertion, white, 4 an allocation", |w| audited(IncHeap::new(w, Barrier::Insertion, Pace::Alloc(4), MARK_STACK))),
    ("insertion, black, 4 an allocation", |w| {
        audited(IncHeap::new(w, Barrier::Insertion, Pace::Alloc(4), MARK_STACK).born(Birth::Black))
    }),
    ("deletion, 4 an allocation", |w| audited(IncHeap::new(w, Barrier::Deletion, Pace::Alloc(4), MARK_STACK))),
    ("insertion, 4 an allocation, a stack of 2", |w| audited(IncHeap::new(w, Barrier::Insertion, Pace::Alloc(4), 2))),
    ("deletion, 1 an allocation, a stack of 2", |w| audited(IncHeap::new(w, Barrier::Deletion, Pace::Alloc(1), 2))),
    ("deletion, whole", |w| audited(IncHeap::new(w, Barrier::Deletion, Pace::Whole, MARK_STACK))),
    ("insertion, 32 a tick", |w| {
        let mut heap = audited(IncHeap::new(w, Barrier::Insertion, Pace::Tick(32), MARK_STACK));
        heap.set_ticker(every_twentieth);
        heap
    }),
    ("deletion, 32 a tick", |w| {
        let mut heap = audited(IncHeap::new(w, Barrier::Deletion, Pace::Tick(32), MARK_STACK));
        heap.set_ticker(every_twentieth);
        heap
    }),
    ("deletion, a slice of 40 a tick", |w| {
        let mut heap = audited(IncHeap::new(w, Barrier::Deletion, Pace::Slice(40), MARK_STACK));
        heap.set_ticker(every_twentieth);
        heap.set_clock(counting_clock);
        heap
    }),
];

struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n
    }
}

/// What the root stack reaches, with objects named by the order a walk meets them.
fn shape<H: Heap>(heap: &H) -> String {
    use std::collections::BTreeMap;
    use std::fmt::Write;
    let mut names: BTreeMap<u32, usize> = BTreeMap::new();
    let mut order: Vec<Obj> = Vec::new();
    let mut out = String::new();
    let mut name = |v: Value, order: &mut Vec<Obj>| match v.as_obj() {
        None => std::format!("{v:?}"),
        Some(obj) => {
            let fresh = names.len();
            let n = *names.entry(obj.0).or_insert_with(|| {
                order.push(obj);
                fresh
            });
            std::format!("#{n}")
        }
    };
    for slot in 0..heap.roots() {
        let _ = write!(out, "{} ", name(heap.root(slot), &mut order));
    }
    let mut done = 0;
    while done < order.len() {
        let obj = order[done];
        let _ = write!(out, "| {:?}", heap.kind(obj));
        for i in 0..heap.kind(obj).fields() {
            let _ = write!(out, " {}", name(heap.field(obj, i), &mut order));
        }
        done += 1;
    }
    out
}

/// One step of a random program. It is the one of `gc_tests.rs` (allocate, overwrite a
/// field with a value it holds, overwrite a root slot, pop) with two things added that a
/// cycle in steps needs to be tested by. It builds lists, so that marking takes many
/// steps. And it reads fields: it walks from a root slot through a few objects and
/// pushes what it finds, which is the only way to hold a reference to an object that the
/// start of a cycle didn't shade.
fn mutate(heap: &mut IncHeap, random: &mut Random) {
    let roots = heap.roots();
    let held = |heap: &IncHeap, random: &mut Random| {
        if roots == 0 || random.below(4) == 0 { Value::int(7) } else { heap.root(random.below(roots)) }
    };
    match random.below(16) {
        0 if roots < 60 => {
            let kind = [Kind::Pair, Kind::Closure, Kind::Binding][random.below(3)];
            for _ in 0..kind.fields() {
                let v = held(heap, random);
                heap.push(v);
            }
            heap.alloc(kind).unwrap();
        }
        1 if roots < 30 => list(heap, 100),
        2..=6 if roots > 0 => {
            if let Some(obj) = heap.root(random.below(roots)).as_obj() {
                // Mostly the first field, so that most lists stay lists.
                let field = if random.below(4) == 0 { random.below(heap.kind(obj).fields()) } else { 0 };
                let v = held(heap, random);
                heap.set_field(obj, field, v);
            }
        }
        7 if roots > 0 => {
            let v = held(heap, random);
            heap.set_root(random.below(roots), v);
        }
        8..=12 if roots > 0 && roots < 60 => {
            let mut v = heap.root(random.below(roots));
            for _ in 0..random.below(100) {
                match v.as_obj() {
                    Some(obj) => v = heap.field(obj, if random.below(4) == 0 { 0 } else { 1 }),
                    None => break,
                }
            }
            heap.push(v);
        }
        _ if roots > 0 => heap.pop(),
        _ => {}
    }
}

/// No cycle may lose an object, and `collect` must leave what the root stack reaches
/// and no more.
#[test]
fn cycles_interleaved_with_a_random_program_lose_nothing() {
    for (name, make) in HEAPS {
        for seed in 0..20 {
            let mut random = Random(seed);
            let mut heap = make(20000 + 200 * random.below(10));
            heap.set_stress(seed % 2 == 0); // on even seeds, a cycle is in progress all the time
            for step in 0..3000 {
                mutate(&mut heap, &mut random);
                assert_eq!(heap.steps().lost, 0, "{name}, seed {seed}, step {step}");
                if step % 500 == 499 {
                    let before = shape(&heap);
                    heap.collect();
                    assert_eq!(shape(&heap), before, "{name}, seed {seed}, step {step}");
                    let (objects, words) = reachable(&heap);
                    assert_eq!((heap.stats().live(), heap.stats().live_words), (objects, words), "{name}, {seed}");
                }
            }
            assert!(heap.counts().collections > 5, "{name}: {}", heap.counts().collections);
            while heap.roots() > 0 {
                heap.pop();
            }
            heap.collect();
            assert_eq!(heap.stats().live(), 0, "{name}, seed {seed}");
        }
    }
}

#[test]
fn the_language_is_the_same_with_a_cycle_always_in_progress() {
    let sources = [
        "(quote (a b (c 1) nil))",
        "(define p (cons 1 2)) (set-car! p 5) (set-cdr! p (cons 6 nil)) p",
        "(define x 5) (define x (cons x x)) x",
        "((lambda (a b) (cons b (cons a nil))) (cons 1 2) (cons 3 4))",
        "(define (adder n) (lambda (x) (+ x n))) (define add3 (adder 3)) (cons (add3 4) (add3 5))",
        "(define (map f l) (if (null? l) nil (cons (f (car l)) (map f (cdr l))))) (map (lambda (x) (* x x)) '(1 2 3 4))",
        "(define p (cons 1 nil)) (set-cdr! p p) p",
        "(define p (cons 1 2)) (define q (cons p p)) (eq? (car q) (cdr q))",
    ];
    for (name, make) in HEAPS {
        for source in sources {
            let expected = Lisp::new(RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak)).run(source).unwrap();
            let mut lisp = Lisp::new(make(WORDS));
            lisp.heap.set_stress(true);
            assert_eq!(lisp.run(source).unwrap(), expected, "{name}: {source}");
            assert_eq!(lisp.heap.steps().lost, 0, "{name}: {source}");
        }
    }
}

#[test]
fn every_program_computes_its_value_and_leaves_nothing() {
    for (name, make) in HEAPS {
        for program in PROGRAMS.iter().chain(&MUTATORS) {
            for (words, stress) in [(4096, false), (WORDS, true)] {
                let mut lisp = Lisp::new(make(words));
                lisp.heap.set_stress(stress);
                assert_eq!(lisp.run(program.source).unwrap(), program.value, "{name}: {} in {words}", program.name);
                assert_eq!(lisp.heap.roots(), 1);
                assert_eq!(lisp.heap.steps().lost, 0, "{name}: {}", program.name);
                assert!(lisp.heap.counts().collections > 0, "{name}: {}", program.name);
                lisp.heap.collect();
                let (objects, words) = reachable(&lisp.heap);
                assert_eq!((lisp.heap.stats().live(), lisp.heap.stats().live_words), (objects, words));
                lisp.forget_globals();
                lisp.heap.collect();
                assert_eq!(lisp.heap.stats().live(), 0, "{name}: {}", program.name);
                assert_eq!(lisp.heap.stats().freed, lisp.heap.stats().allocated);
            }
        }
    }
}

/// `nreverse` turns round the references the mark phase is following. Without a barrier
/// the audit finds reachable objects unmarked. The heap is told to stop there: a program
/// that went on, with part of its list freed and allocated again, might never end.
#[test]
fn nreverse_without_a_barrier_loses_part_of_its_list() {
    let mut lisp = Lisp::new(audited(IncHeap::new(4096, Barrier::None, Pace::Alloc(4), MARK_STACK)));
    lisp.heap.set_strict(true);
    let value = catch_unwind(AssertUnwindSafe(|| lisp.run(MUTATORS[0].source)));
    assert!(value.is_err());
    assert!(lisp.heap.steps().lost > 0);
}

/// Objects born white have to be found by a scan of the root stack, and each scan finds
/// the ones born since the last. At one unit of work for each allocation the scans use
/// the whole budget, and marking never ends. Born black, they need no finding.
#[test]
fn marking_objects_born_white_must_outrun_the_program() {
    let cycles = |barrier, birth, units| {
        let mut lisp = Lisp::new(IncHeap::new(WORDS, barrier, Pace::Alloc(units), MARK_STACK).born(birth));
        lisp.heap.set_stress(true);
        assert_eq!(lisp.run(PROGRAMS[0].source).unwrap(), PROGRAMS[0].value);
        lisp.heap.counts().collections
    };
    assert_eq!(cycles(Barrier::Insertion, Birth::White, 1), 0);
    assert!(cycles(Barrier::Insertion, Birth::White, 2) > 0);
    assert!(cycles(Barrier::Insertion, Birth::Black, 1) > 0);
    assert!(cycles(Barrier::Deletion, Birth::Black, 1) > 0);
}

#[test]
fn an_error_leaves_the_interpreter_usable() {
    let mut lisp = Lisp::new(IncHeap::new(400, Barrier::Deletion, Pace::Alloc(4), MARK_STACK));
    let source = "(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc)))) (iota 200 nil)";
    assert_eq!(lisp.run(source), Err(Error::HeapFull));
    assert_eq!(lisp.heap.roots(), 1);
    assert_eq!(lisp.run("(iota 5 nil)").unwrap(), "(1 2 3 4 5)");
}

/// With the same pace and no timer, every count is the same on every run.
#[test]
fn two_runs_count_the_same() {
    for (_, make) in &HEAPS[..5] {
        let run = || {
            let mut lisp = Lisp::new(make(4096));
            lisp.run(MUTATORS[0].source).unwrap();
            (lisp.heap.steps(), lisp.heap.counts(), lisp.heap.stats())
        };
        assert_eq!(run(), run());
    }
}
