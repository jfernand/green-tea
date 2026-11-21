//! Tests of the generational heap. The first group is the tests of the tracing heaps,
//! written again for a heap whose `collect` is two collections, and run with a small
//! nursery under each barrier, with a nursery of eight words, and with the nursery that
//! is half of the free words. The second group is about what only this heap has: the
//! write barrier, the remembered set, and the two kinds of collection.

use super::{Barrier, GenHeap, MUTATING};
use crate::gc::{Collect, reachable};
use crate::programs::PROGRAMS;
use crate::rc::{Cycles, Freeing, RcHeap};
use crate::semispace::Semispace;
use crate::{Error, Full, Heap, Kind, Lisp, Obj, Value};
use std::string::String;
use std::vec::Vec;

const WORDS: usize = 1 << 16;

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

fn top<H: Heap>(heap: &H) -> Obj {
    heap.top().as_obj().unwrap()
}

/// The elements of the list in a root slot.
fn elements<H: Heap>(heap: &H, slot: usize) -> Vec<Value> {
    let (mut out, mut rest) = (Vec::new(), heap.root(slot));
    while let Some(pair) = rest.as_obj() {
        out.push(heap.field(pair, 0));
        rest = heap.field(pair, 1);
    }
    out
}

/// What the root stack reaches, with each object named by the order in which a walk from
/// the root stack first meets it: the same text wherever the objects are.
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

struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n
    }
}

type Make = fn(usize) -> GenHeap;

// ---- the tests of any tracing heap ----------------------------------------------------

fn a_collection_frees_what_the_roots_cannot_reach(make: Make) {
    let mut heap = make(WORDS);
    list(&mut heap, 10);
    list(&mut heap, 5);
    heap.pop(); // nothing refers to the second list now, and nothing is freed
    assert_eq!((heap.stats().allocated, heap.stats().freed), (15, 0));
    heap.collect();
    assert_eq!((heap.stats().freed, heap.stats().live(), heap.stats().live_words), (5, 10, 30));
    assert_eq!(reachable(&heap), (10, 30));
    assert_eq!(elements(&heap, 0), (0..10).rev().map(Value::int).collect::<Vec<_>>());
    // The minor collection copied the ten, and the major one marked the same ten. A
    // nursery too small for the lists had minor collections before this, which copied
    // some of the five as well.
    let (counts, generations) = (heap.counts(), heap.generations());
    assert_eq!((generations.major, generations.marked), (1, 10));
    assert_eq!(counts.traced, generations.promoted + 10);
    if heap.nursery_words().0 >= 45 {
        assert_eq!((counts.collections, counts.roots, generations.minor, generations.promoted), (2, 2, 1, 10));
    }
    heap.check();
    heap.pop();
    heap.collect();
    assert_eq!((heap.stats().live(), heap.stats().live_words), (0, 0));
}

fn a_cycle_that_nothing_refers_to_is_freed(make: Make) {
    let mut heap = make(WORDS);
    list(&mut heap, 8);
    let first = top(&heap);
    let mut last = first;
    while let Some(next) = heap.field(last, 1).as_obj() {
        last = next;
    }
    heap.set_field(last, 1, Value::obj(first));
    heap.collect();
    assert_eq!((heap.stats().live(), heap.generations().promoted), (8, 8)); // copied once each
    heap.pop();
    heap.collect();
    assert_eq!(heap.stats().live(), 0);
}

fn alloc_collects_when_the_heap_is_full(make: Make) {
    // 60 words, and a list of 8 pairs is 24. 1,000 such lists go through it.
    let mut heap = make(60);
    for _ in 0..1000 {
        list(&mut heap, 8);
        assert_eq!(elements(&heap, 0), (0..8).rev().map(Value::int).collect::<Vec<_>>());
        heap.pop();
    }
    assert_eq!(heap.stats().allocated, 8000);
    assert!(heap.generations().minor >= 400, "{}", heap.generations().minor);
    assert!(heap.stats().peak_words <= 60);
    heap.check();
}

fn a_full_heap_is_an_error_and_the_roots_are_kept(make: Make) {
    // Cons onto a list until the heap refuses. The old space can take all but the last
    // few of the 60 words: what is left has to be a nursery and room for its survivors.
    let mut heap = make(60);
    heap.push(Value::NIL);
    let mut pairs = 0;
    loop {
        heap.push(Value::int(pairs));
        heap.push(heap.root(0));
        let before = shape(&heap);
        if heap.alloc(Kind::Pair) == Err(Full) {
            assert_eq!(shape(&heap), before);
            break;
        }
        heap.set_root(0, heap.top());
        heap.pop();
        pairs += 1;
    }
    assert!((17..=19).contains(&pairs), "{pairs}");
    heap.pop();
    heap.pop();
    assert_eq!(elements(&heap, 0), (0..pairs).rev().map(Value::int).collect::<Vec<_>>());
    assert_eq!(heap.stats().live(), pairs as u64);
}

fn the_fields_of_a_new_object_survive_the_collection_it_starts(make: Make) {
    let mut heap = make(WORDS);
    list(&mut heap, 3);
    list(&mut heap, 2);
    heap.set_stress(true);
    heap.alloc(Kind::Pair).unwrap(); // collects first, with both lists on the root stack
    let pair = top(&heap);
    let lengths = |v: Value| {
        let (mut n, mut rest) = (0, v);
        while let Some(p) = rest.as_obj() {
            n += 1;
            rest = heap.field(p, 1);
        }
        n
    };
    assert_eq!((lengths(heap.field(pair, 0)), lengths(heap.field(pair, 1))), (3, 2));
    assert_eq!(heap.generations().major, 1);
}

/// A random program against the interface: it allocates, overwrites fields with whatever
/// it holds (which stores young references in old objects, and makes cycles), and drops
/// roots. The heap is small enough that `alloc` collects on its own, minor and major.
/// After every step the heap must pass its own check, and a collection the test asks for
/// must change nothing that the root stack can reach and leave nothing else allocated.
fn a_collection_keeps_the_graph_and_nothing_else(make: Make) {
    for seed in 0..20 {
        let mut random = Random(seed);
        let mut heap = make(2000 + 200 * random.below(10));
        for step in 0..3000 {
            let roots = heap.roots();
            let held = |heap: &GenHeap, random: &mut Random| {
                if roots == 0 || random.below(4) == 0 { Value::int(7) } else { heap.root(random.below(roots)) }
            };
            match random.below(6) {
                0 | 1 if roots < 60 => {
                    let kind = [Kind::Pair, Kind::Closure, Kind::Binding][random.below(3)];
                    for _ in 0..kind.fields() {
                        let v = held(&heap, &mut random);
                        heap.push(v);
                    }
                    heap.alloc(kind).unwrap();
                }
                2 | 3 if roots > 0 => {
                    if let Some(obj) = heap.root(random.below(roots)).as_obj() {
                        let field = random.below(heap.kind(obj).fields());
                        let v = held(&heap, &mut random);
                        heap.set_field(obj, field, v);
                    }
                }
                4 if roots > 0 => {
                    let v = held(&heap, &mut random);
                    heap.set_root(random.below(roots), v);
                }
                _ if roots > 0 => heap.pop(),
                _ => {}
            }
            heap.check();
            if step % 100 == 99 {
                let before = shape(&heap);
                if step % 200 == 99 {
                    heap.minor();
                    assert_eq!(shape(&heap), before, "seed {seed}, step {step}, minor");
                    heap.check();
                }
                heap.collect();
                assert_eq!(shape(&heap), before, "seed {seed}, step {step}");
                let (objects, words) = reachable(&heap);
                assert_eq!(
                    (heap.stats().live(), heap.stats().live_words),
                    (objects, words),
                    "seed {seed}, step {step}"
                );
            }
        }
        let generations = heap.generations();
        assert!(generations.minor > 20 && generations.major > 15, "seed {seed}: {generations:?}");
        assert!(generations.remembered_found > 0, "seed {seed}: no old object referred to the nursery");
        while heap.roots() > 0 {
            heap.pop();
        }
        heap.collect();
        assert_eq!(heap.stats().live(), 0, "seed {seed}");
    }
}

fn the_language_is_the_same_under_a_collection_at_every_allocation(make: Make) {
    let sources = [
        "(quote (a b (c 1) nil))",
        "(cons 1 (cons 2 nil))",
        "(define p (cons 1 2)) (set-car! p 5) (set-cdr! p (cons 6 nil)) p",
        "(define x 5) (define x (cons x x)) x",
        "((lambda (a b) (cons b (cons a nil))) (cons 1 2) (cons 3 4))",
        "(define (adder n) (lambda (x) (+ x n))) (define add3 (adder 3)) (cons (add3 4) (add3 5))",
        "(define (f) (cons 1 2) (cons 3 4) (cons 5 6)) (f)",
        "(define (map f l) (if (null? l) nil (cons (f (car l)) (map f (cdr l))))) (map (lambda (x) (* x x)) '(1 2 3 4))",
        "(define p (cons 1 nil)) (set-cdr! p p) p",
        "(if (cons 1 2) (cons 3 4) (cons 5 6))",
        "(eq? (cons 1 2) (cons 1 2))",
        "(define p (cons 1 2)) (define q (cons p p)) (eq? (car q) (cdr q))",
    ];
    for source in sources {
        let expected = Lisp::new(RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak)).run(source).unwrap();
        // A full collection at every allocation, and then no collection but the minor
        // ones of a nursery that holds two objects.
        let mut lisp = Lisp::new(make(WORDS));
        lisp.heap.set_stress(true);
        assert_eq!(lisp.run(source).unwrap(), expected, "{source}");
        assert!(lisp.heap.counts().collections > 0);
        lisp.heap.check();
        let mut lisp = Lisp::new(GenHeap::new(WORDS, 8, Barrier::Filter));
        assert_eq!(lisp.run(source).unwrap(), expected, "{source}");
        assert!(lisp.heap.generations().minor > 0 && lisp.heap.generations().major == 0);
        lisp.heap.check();
    }
}

fn an_error_leaves_the_interpreter_usable(make: Make) {
    let mut lisp = Lisp::new(make(400));
    let source = "(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc)))) (iota 200 nil)";
    assert_eq!(lisp.run(source), Err(Error::HeapFull));
    assert_eq!(lisp.heap.roots(), 1);
    assert_eq!(lisp.run("(iota 5 nil)").unwrap(), "(1 2 3 4 5)");
}

fn every_program_computes_its_value_and_leaves_nothing(make: Make) {
    for program in PROGRAMS.iter().chain(&MUTATING) {
        for (words, stress) in [(WORDS, false), (4096, false), (1024, false), (WORDS, true)] {
            let mut lisp = Lisp::new(make(words));
            lisp.heap.set_stress(stress);
            assert_eq!(lisp.run(program.source).unwrap(), program.value, "{} in {words}, {stress}", program.name);
            assert_eq!(lisp.heap.roots(), 1);
            lisp.heap.check();
            lisp.heap.collect();
            let (objects, words) = reachable(&lisp.heap);
            assert_eq!((lisp.heap.stats().live(), lisp.heap.stats().live_words), (objects, words));
            lisp.forget_globals();
            lisp.heap.collect();
            // Rings and closures that call themselves included.
            assert_eq!(lisp.heap.stats().live(), 0, "{}", program.name);
            assert_eq!(lisp.heap.stats().freed, lisp.heap.stats().allocated);
            // Every object died in the nursery, or was promoted and died in the old space.
            let generations = lisp.heap.generations();
            assert_eq!(generations.promoted, generations.old_freed, "{}", program.name);
        }
    }
}

fn the_same_program_allocates_the_same_on_every_heap(make: Make) {
    for program in PROGRAMS.iter().chain(&MUTATING) {
        let mut counting = Lisp::new(RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak));
        counting.run(program.source).unwrap();
        let mut lisp = Lisp::new(make(WORDS));
        lisp.run(program.source).unwrap();
        assert_eq!(lisp.heap.stats().allocated, counting.heap.stats().allocated, "{}", program.name);
    }
}

macro_rules! on_every_heap {
    ($($test:ident),* $(,)?) => {
        mod filter {
            use super::*;
            $(#[test] fn $test() { super::$test(|words| GenHeap::new(words, 64, Barrier::Filter)) })*
        }
        mod every {
            use super::*;
            $(#[test] fn $test() { super::$test(|words| GenHeap::new(words, 64, Barrier::Every)) })*
        }
        /// A nursery of two objects: nearly every allocation is a minor collection.
        mod with_a_nursery_of_eight_words {
            use super::*;
            $(#[test] fn $test() { super::$test(|words| GenHeap::new(words, 8, Barrier::Filter)) })*
        }
        /// No limit on the nursery: it is half of what the old space isn't using.
        mod with_half_of_the_free_words {
            use super::*;
            $(#[test] fn $test() { super::$test(|words| GenHeap::new(words, usize::MAX, Barrier::Filter)) })*
        }
    };
}

on_every_heap!(
    a_collection_frees_what_the_roots_cannot_reach,
    a_cycle_that_nothing_refers_to_is_freed,
    alloc_collects_when_the_heap_is_full,
    a_full_heap_is_an_error_and_the_roots_are_kept,
    the_fields_of_a_new_object_survive_the_collection_it_starts,
    a_collection_keeps_the_graph_and_nothing_else,
    the_language_is_the_same_under_a_collection_at_every_allocation,
    an_error_leaves_the_interpreter_usable,
    every_program_computes_its_value_and_leaves_nothing,
    the_same_program_allocates_the_same_on_every_heap,
);

// ---- the two generations --------------------------------------------------------------

/// A heap of 4,096 words with a nursery of 64: room for 21 pairs.
fn small() -> GenHeap {
    GenHeap::new(4096, 64, Barrier::Filter)
}

#[test]
fn every_object_is_born_in_the_nursery_and_a_survivor_moves_to_the_old_space() {
    let mut heap = small();
    list(&mut heap, 4);
    let young = top(&heap);
    assert!(heap.is_young(young));
    assert_eq!((heap.old_words(), heap.nursery_words()), (0, (64, 12)));
    heap.minor();
    let old = top(&heap);
    assert!(!heap.is_young(old));
    // Cheney's order: the list's first pair is the first object of the old space.
    assert_eq!(old, Obj(1));
    assert_eq!(heap.field(old, 1), Value::obj(Obj(4)));
    assert_eq!((heap.old_words(), heap.nursery_words()), (12, (64, 0)));
    assert_eq!(elements(&heap, 0), (0..4).rev().map(Value::int).collect::<Vec<_>>());
}

#[test]
fn a_minor_collection_does_not_trace_the_old_space() {
    let mut heap = GenHeap::new(WORDS, 64, Barrier::Filter);
    list(&mut heap, 1000); // 48 minor collections on the way, and then one more
    heap.minor();
    let before = heap.counts();
    assert_eq!((heap.generations().promoted, heap.generations().major), (1000, 0));
    // Twenty pairs of garbage, and a collection with 1,000 reachable objects in the heap.
    list(&mut heap, 20);
    heap.pop();
    heap.minor();
    let after = heap.counts();
    assert_eq!((after.traced - before.traced, after.walked, after.roots - before.roots), (0, 0, 1));
    assert_eq!(heap.stats().live(), 1000);
    assert_eq!(elements(&heap, 0).len(), 1000);
}

#[test]
fn an_old_object_made_to_refer_to_a_young_one_is_remembered_once() {
    let mut heap = small();
    list(&mut heap, 1);
    heap.minor();
    let old = top(&heap);
    list(&mut heap, 1);
    list(&mut heap, 1);
    let (a, b) = (heap.root(1), heap.root(2));
    heap.set_field(old, 0, a);
    heap.set_field(old, 1, b);
    heap.set_field(old, 0, b);
    let generations = heap.generations();
    assert_eq!((generations.stores, generations.stores_old, generations.old_to_young), (3, 3, 3));
    assert_eq!((generations.recorded, heap.remembered()), (1, 1));
    heap.check();
    // The old pair is all that refers to the second young one, once the roots let go.
    heap.pop();
    heap.pop();
    heap.minor();
    assert_eq!((heap.generations().promoted, heap.remembered()), (2, 0));
    let old = top(&heap);
    let kept = heap.field(old, 0).as_obj().unwrap();
    assert!(!heap.is_young(kept));
    assert_eq!((heap.field(old, 1), heap.field(kept, 0)), (Value::obj(kept), Value::int(0)));
    let generations = heap.generations();
    assert_eq!((generations.remembered_scanned, generations.remembered_found), (1, 2));
    heap.check();
}

#[test]
fn stores_that_make_no_reference_from_old_to_young_are_not_recorded() {
    let mut heap = small();
    list(&mut heap, 2);
    heap.minor();
    let old = top(&heap);
    let older = heap.field(old, 1);
    list(&mut heap, 2);
    let young = top(&heap);
    heap.set_field(old, 0, Value::int(5)); // an integer into an old object
    heap.set_field(old, 0, older); // an old reference into an old object
    heap.set_field(young, 0, Value::obj(old)); // an old reference into a young object
    heap.set_field(young, 0, heap.field(young, 1)); // a young reference into a young object
    let generations = heap.generations();
    assert_eq!((generations.stores, generations.stores_old, generations.old_to_young), (4, 2, 0));
    assert_eq!((generations.recorded, heap.remembered()), (0, 0));
    heap.check();
}

#[test]
fn the_barrier_that_tests_nothing_records_every_store() {
    let mut heap = GenHeap::new(4096, 64, Barrier::Every);
    list(&mut heap, 2);
    heap.minor();
    let old = top(&heap);
    list(&mut heap, 2);
    let young = top(&heap);
    heap.set_field(old, 0, Value::int(5));
    heap.set_field(old, 0, Value::obj(young));
    heap.set_field(old, 0, Value::obj(young));
    heap.set_field(young, 0, Value::int(6));
    assert_eq!((heap.generations().recorded, heap.remembered()), (4, 4));
    heap.check();
    heap.pop();
    heap.minor();
    // Four entries read: one was a nursery object, and three were the same old pair.
    let generations = heap.generations();
    assert_eq!((generations.remembered_read, generations.remembered_scanned, generations.remembered_found), (4, 3, 1));
    assert_eq!((generations.promoted, heap.remembered()), (4, 0));
    heap.check();
}

#[test]
fn a_new_object_that_refers_to_old_ones_needs_no_barrier() {
    let mut heap = small();
    list(&mut heap, 5);
    heap.minor();
    // A pair whose two fields refer to old objects, filled in by `alloc`.
    heap.push(heap.top());
    heap.push(heap.top());
    heap.alloc(Kind::Pair).unwrap();
    assert_eq!((heap.generations().stores, heap.remembered()), (0, 0));
    heap.check(); // no old object refers to the nursery
    heap.minor();
    assert_eq!(heap.generations().promoted, 6);
    heap.check();
}

#[test]
fn both_barriers_promote_the_same_objects() {
    for program in PROGRAMS.iter().chain(&MUTATING) {
        let run = |barrier| {
            let mut lisp = Lisp::new(GenHeap::new(4096, 256, barrier));
            assert_eq!(lisp.run(program.source).unwrap(), program.value);
            (lisp.heap.generations(), lisp.heap.stats())
        };
        let ((filter, a), (every, b)) = (run(Barrier::Filter), run(Barrier::Every));
        assert_eq!(a, b, "{}", program.name);
        assert_eq!(
            (filter.minor, filter.major, filter.promoted, filter.marked, filter.stores, filter.remembered_found),
            (every.minor, every.major, every.promoted, every.marked, every.stores, every.remembered_found),
            "{}",
            program.name
        );
        assert_eq!(every.recorded, every.stores);
        assert!(filter.recorded <= filter.old_to_young && filter.old_to_young <= filter.stores_old);
    }
}

#[test]
#[should_panic(expected = "is not remembered")]
fn a_store_without_the_barrier_fails_the_check() {
    let mut heap = small();
    list(&mut heap, 1);
    heap.minor();
    let old = top(&heap);
    list(&mut heap, 1);
    heap.store_without_barrier(old, 0, heap.top());
    heap.check();
}

#[test]
fn a_store_without_the_barrier_leaves_a_reference_to_another_object() {
    let mut heap = small();
    list(&mut heap, 1);
    heap.minor();
    let old = top(&heap);
    // A young pair (41 . 42) that only the old pair refers to, as far as anyone can tell.
    heap.push(Value::int(41));
    heap.push(Value::int(42));
    heap.alloc(Kind::Pair).unwrap();
    heap.store_without_barrier(old, 0, heap.top());
    heap.pop();
    heap.minor(); // nothing leads to the young pair: it is not copied
    assert_eq!(heap.generations().promoted, 1);
    let stale = heap.field(top(&heap), 0).as_obj().unwrap();
    assert!(heap.is_young(stale));
    // The nursery is empty, and the next pair made is where the lost one was.
    heap.push(Value::int(3));
    heap.push(Value::int(4));
    heap.alloc(Kind::Pair).unwrap();
    assert_eq!((heap.field(stale, 0), heap.field(stale, 1)), (Value::int(3), Value::int(4)));
}

#[test]
fn the_nursery_is_half_of_the_free_words_when_that_is_less_than_its_size() {
    let mut heap = GenHeap::new(1000, 400, Barrier::Filter);
    assert_eq!(heap.nursery_words(), (400, 0));
    list(&mut heap, 100); // 300 words, kept
    heap.minor();
    assert_eq!((heap.old_words(), heap.nursery_words()), (300, (350, 0)));
    // With no limit, it is always half: the heap of Appel's paper.
    let mut heap = GenHeap::new(1000, usize::MAX, Barrier::Filter);
    assert_eq!(heap.nursery_words(), (500, 0));
    list(&mut heap, 100);
    heap.minor();
    assert_eq!((heap.old_words(), heap.nursery_words()), (300, (350, 0)));
}

#[test]
fn a_major_collection_gives_the_nursery_the_words_it_freed() {
    let mut heap = GenHeap::new(1000, 400, Barrier::Filter);
    list(&mut heap, 100);
    heap.minor();
    assert_eq!((heap.old_words(), heap.nursery_words()), (300, (350, 0)));
    heap.pop(); // the list is garbage in the old space
    heap.major();
    assert_eq!((heap.old_words(), heap.nursery_words()), (0, (400, 0)));
}

#[test]
fn a_major_collection_follows_the_minor_one_that_leaves_the_old_space_over_half_the_heap() {
    let mut heap = GenHeap::new(1200, 60, Barrier::Filter);
    // Lists of 10 pairs, each kept until the next is made: every minor collection promotes
    // one or two of them, and all but the last is dead by then.
    heap.push(Value::NIL);
    let mut rounds = 0;
    while heap.generations().major == 0 {
        list(&mut heap, 10);
        heap.set_root(0, heap.top());
        heap.pop();
        rounds += 1;
    }
    let generations = heap.generations();
    // 600 words of promoted pairs is 200 of them.
    assert!(generations.promoted > 200 && generations.promoted <= 220, "{}", generations.promoted);
    assert_eq!(generations.promoted - generations.old_freed, generations.marked);
    assert!(generations.marked <= 20 && heap.old_words() <= 60, "{}", generations.marked);
    assert!(rounds > 20);
    assert_eq!(heap.counts().walked, 3 * generations.promoted);
    heap.check();
}

#[test]
fn a_reference_kept_across_a_minor_collection_is_caught_until_the_nursery_is_used_again() {
    let mut heap = small();
    list(&mut heap, 1);
    let borrowed = top(&heap);
    heap.minor();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| heap.field(borrowed, 0)));
    assert!(caught.is_err());
    list(&mut heap, 1);
    assert_eq!(heap.field(borrowed, 0), Value::int(0)); // some other pair
}

/// The smallest heap, in words, that each program computes its value in, with a nursery
/// of 256 words at most. The example `generations` finds them.
const SMALLEST: [(&str, usize); 5] = [("list", 618), ("fib", 138), ("alist", 769), ("ring", 400), ("closure", 241)];

#[test]
fn each_program_fits_in_its_smallest_heap_and_not_in_a_word_less() {
    for (program, (name, smallest)) in PROGRAMS.iter().zip(SMALLEST) {
        assert_eq!(program.name, name);
        for (words, fitting) in [(smallest, true), (smallest - 1, false)] {
            let result = Lisp::new(GenHeap::new(words, 256, Barrier::Filter)).run(program.source);
            assert!(result.as_deref() == Ok(program.value) || result == Err(Error::HeapFull));
            assert_eq!(result.is_ok(), fitting, "{name} in {words}");
        }
        // A heap that copies needs twice the most words the program has reachable at once.
        // This one needs those words once, and three or four more: the old space is
        // compacted where it is, and the nursery and the room for its survivors shrink
        // to an object each.
        let half = (1..).find(|&w| Lisp::new(Semispace::new(2 * w)).run(program.source).is_ok()).unwrap();
        assert!(smallest == half + 3 || smallest == half + 4, "{name}: {smallest} and {half}");
    }
}
