//! Tests of the tracing heaps. Each test is written once, against [`Collect`], and run on
//! the mark-sweep heap, the mark-compact heap and the copying heap.

use crate::gc::{Collect, reachable};
use crate::mark::{MarkHeap, Reclaim};
use crate::programs::PROGRAMS;
use crate::rc::{Cycles, Freeing, RcHeap};
use crate::semispace::Semispace;
use crate::{Error, Full, Heap, Kind, Lisp, Obj, Value};
use std::string::String;
use std::vec::Vec;

const WORDS: usize = 1 << 16;
const MARK_STACK: usize = 1024;

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

/// What the root stack reaches, written so that two heaps that hold the same graph give
/// the same text, wherever their objects are: an object is named by the order in which a
/// walk from the root stack first meets it.
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

// ---- the tests, for any heap ---------------------------------------------------------

fn a_collection_frees_what_the_roots_cannot_reach<H: Collect>(make: fn(usize) -> H) {
    let mut heap = make(WORDS);
    list(&mut heap, 10);
    list(&mut heap, 5);
    heap.pop(); // nothing refers to the second list now, and nothing is freed
    assert_eq!((heap.stats().allocated, heap.stats().freed), (15, 0));
    heap.collect();
    assert_eq!((heap.stats().freed, heap.stats().live(), heap.stats().live_words), (5, 10, 30));
    assert_eq!(reachable(&heap), (10, 30));
    assert_eq!(elements(&heap, 0), (0..10).rev().map(Value::int).collect::<Vec<_>>());
    let counts = heap.counts();
    assert_eq!((counts.collections, counts.traced, counts.traced_words, counts.roots), (1, 10, 30, 1));
    heap.pop();
    heap.collect();
    assert_eq!((heap.stats().live(), heap.stats().live_words), (0, 0));
}

fn a_cycle_that_nothing_refers_to_is_freed<H: Collect>(make: fn(usize) -> H) {
    let mut heap = make(WORDS);
    list(&mut heap, 8);
    let first = top(&heap);
    let mut last = first;
    while let Some(next) = heap.field(last, 1).as_obj() {
        last = next;
    }
    heap.set_field(last, 1, Value::obj(first));
    heap.collect();
    assert_eq!(heap.stats().live(), 8); // reachable, and marked or copied once each
    assert_eq!(heap.counts().traced, 8);
    heap.pop();
    heap.collect();
    assert_eq!(heap.stats().live(), 0);
}

fn alloc_collects_when_the_heap_is_full<H: Collect>(make: fn(usize) -> H) {
    // Room for 20 pairs, or 10 in each half. 1,000 lists of 8 pairs go through it.
    let mut heap = make(60);
    for _ in 0..1000 {
        list(&mut heap, 8);
        assert_eq!(elements(&heap, 0), (0..8).rev().map(Value::int).collect::<Vec<_>>());
        heap.pop();
    }
    assert_eq!(heap.stats().allocated, 8000);
    assert!(heap.counts().collections >= 400, "{}", heap.counts().collections);
    assert!(heap.stats().peak_words <= 60);
}

fn a_full_heap_is_an_error_and_the_roots_are_kept<H: Collect>(make: fn(usize) -> H) {
    // Cons onto a list until the heap refuses: 20 pairs fit in 60 words, or 10 in half of them.
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
    assert!(pairs == 20 || pairs == 10, "{pairs}");
    heap.pop();
    heap.pop();
    assert_eq!(elements(&heap, 0), (0..pairs).rev().map(Value::int).collect::<Vec<_>>());
    assert_eq!(heap.stats().live(), pairs as u64);
}

fn the_fields_of_a_new_object_survive_the_collection_it_starts<H: Collect>(make: fn(usize) -> H) {
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
    assert_eq!(heap.counts().collections, 1);
}

/// A random program against the interface: it allocates, overwrites fields with whatever
/// it holds (which makes cycles), and drops roots. A collection must change nothing that
/// the root stack can reach, and must leave nothing else allocated.
fn a_collection_keeps_the_graph_and_nothing_else<H: Collect>(make: fn(usize) -> H) {
    for seed in 0..20 {
        let mut random = Random(seed);
        // Small enough that `alloc` collects on its own as well.
        let mut heap = make(2000 + 200 * random.below(10));
        for step in 0..3000 {
            let roots = heap.roots();
            let held = |heap: &H, random: &mut Random| {
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
            if step % 100 == 99 {
                let before = shape(&heap);
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
        while heap.roots() > 0 {
            heap.pop();
        }
        heap.collect();
        assert_eq!(heap.stats().live(), 0, "seed {seed}");
    }
}

fn the_language_is_the_same_under_a_collection_at_every_allocation<H: Collect>(make: fn(usize) -> H) {
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
        let mut lisp = Lisp::new(make(WORDS));
        lisp.heap.set_stress(true);
        assert_eq!(lisp.run(source).unwrap(), expected, "{source}");
        assert!(lisp.heap.counts().collections > 0);
    }
}

fn an_error_leaves_the_interpreter_usable<H: Collect>(make: fn(usize) -> H) {
    let mut lisp = Lisp::new(make(400));
    let source = "(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc)))) (iota 200 nil)";
    assert_eq!(lisp.run(source), Err(Error::HeapFull));
    assert_eq!(lisp.heap.roots(), 1);
    assert_eq!(lisp.run("(iota 5 nil)").unwrap(), "(1 2 3 4 5)");
}

fn every_program_computes_its_value_and_leaves_nothing<H: Collect>(make: fn(usize) -> H) {
    for program in &PROGRAMS {
        for (words, stress) in [(WORDS, false), (4096, false), (WORDS, true)] {
            let mut lisp = Lisp::new(make(words));
            lisp.heap.set_stress(stress);
            assert_eq!(lisp.run(program.source).unwrap(), program.value, "{} in {words}, {stress}", program.name);
            assert_eq!(lisp.heap.roots(), 1);
            lisp.heap.collect();
            let (objects, words) = reachable(&lisp.heap);
            assert_eq!((lisp.heap.stats().live(), lisp.heap.stats().live_words), (objects, words));
            lisp.forget_globals();
            lisp.heap.collect();
            // Rings and closures that call themselves included.
            assert_eq!(lisp.heap.stats().live(), 0, "{}", program.name);
            assert_eq!(lisp.heap.stats().freed, lisp.heap.stats().allocated);
        }
    }
}

fn the_same_program_allocates_the_same_on_every_heap<H: Collect>(make: fn(usize) -> H) {
    for program in &PROGRAMS {
        let mut counting = Lisp::new(RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak));
        counting.run(program.source).unwrap();
        let mut lisp = Lisp::new(make(WORDS));
        lisp.run(program.source).unwrap();
        assert_eq!(lisp.heap.stats().allocated, counting.heap.stats().allocated, "{}", program.name);
    }
}

macro_rules! on_every_heap {
    ($($test:ident),* $(,)?) => {
        mod sweep {
            use super::*;
            $(#[test] fn $test() { super::$test(|words| MarkHeap::new(words, Reclaim::Sweep, MARK_STACK)) })*
        }
        mod compact {
            use super::*;
            $(#[test] fn $test() { super::$test(|words| MarkHeap::new(words, Reclaim::Compact, MARK_STACK)) })*
        }
        mod copying {
            use super::*;
            $(#[test] fn $test() { super::$test(Semispace::new) })*
        }
        /// A mark stack of two entries: nearly every collection overflows it.
        mod sweep_with_a_short_mark_stack {
            use super::*;
            $(#[test] fn $test() { super::$test(|words| MarkHeap::new(words, Reclaim::Sweep, 2)) })*
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

/// The smallest heap, in words, that each program computes its value in: on a heap that
/// marks and sweeps, one that marks and compacts, and one that copies. The example `heaps`
/// finds them, by trying every size from one word up.
const SMALLEST: [(&str, [usize; 3]); 5] = [
    ("list", [1278, 614, 1228]),
    ("fib", [149, 134, 268]),
    ("alist", [1459, 766, 1532]),
    ("ring", [429, 396, 792]),
    ("closure", [264, 237, 474]),
];

#[test]
fn each_program_fits_in_its_smallest_heap_and_not_in_a_word_less() {
    fn fits<H: Collect>(heap: H, source: &str, value: &str) -> bool {
        let result = Lisp::new(heap).run(source);
        assert!(result.as_deref() == Ok(value) || result == Err(Error::HeapFull));
        result.is_ok()
    }
    for (program, (name, [sweep, compact, copy])) in PROGRAMS.iter().zip(SMALLEST) {
        assert_eq!(program.name, name);
        let (source, value) = (program.source, program.value);
        for (words, fitting) in [(0, true), (1, false)] {
            assert_eq!(
                fits(MarkHeap::new(sweep - words, Reclaim::Sweep, MARK_STACK), source, value),
                fitting,
                "{name}"
            );
            assert_eq!(
                fits(MarkHeap::new(compact - words, Reclaim::Compact, MARK_STACK), source, value),
                fitting,
                "{name}"
            );
            assert_eq!(fits(Semispace::new(copy - 2 * words), source, value), fitting, "{name}");
        }
        // A heap that compacts needs the most words the program ever has in reachable
        // objects, which is what a heap that counts has in use at its peak, cycles apart.
        // A heap that copies needs that twice.
        assert_eq!(copy, 2 * compact, "{name}");
        if !program.cyclic {
            let mut counting = Lisp::new(RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak));
            counting.run(source).unwrap();
            assert_eq!(counting.heap.stats().peak_words, compact, "{name}");
        }
    }
}

// ---- what differs between them --------------------------------------------------------

#[test]
fn a_sweep_moves_nothing_and_reuses_what_it_freed() {
    let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK);
    list(&mut heap, 4); // garbage, at words 1 to 12
    heap.pop();
    list(&mut heap, 4);
    let kept = top(&heap);
    heap.collect();
    assert_eq!(top(&heap), kept);
    assert_eq!(heap.touched(), 24);
    assert_eq!(heap.counts().walked, 8); // every object, reachable or not
    list(&mut heap, 4); // goes where the garbage was, lowest address first
    assert_eq!(top(&heap), Obj(10));
    assert_eq!(heap.touched(), 24);
}

#[test]
fn a_sweep_cannot_give_a_pair_s_words_to_a_closure() {
    // 30 words, all in pairs, all garbage. A closure needs four words in one piece.
    let mut heap = MarkHeap::new(30, Reclaim::Sweep, MARK_STACK);
    list(&mut heap, 10);
    heap.pop();
    for _ in 0..3 {
        heap.push(Value::NIL);
    }
    assert_eq!(heap.alloc(Kind::Closure), Err(Full));
    assert_eq!(heap.stats().live(), 0);
    // A heap that compacts has the 30 words in one piece.
    let mut heap = MarkHeap::new(30, Reclaim::Compact, MARK_STACK);
    list(&mut heap, 10);
    heap.pop();
    for _ in 0..3 {
        heap.push(Value::NIL);
    }
    assert_eq!(heap.alloc(Kind::Closure), Ok(()));
}

#[test]
fn a_compaction_slides_objects_down_in_order() {
    let mut heap = MarkHeap::new(WORDS, Reclaim::Compact, MARK_STACK);
    list(&mut heap, 4); // garbage, at words 1 to 12
    heap.pop();
    list(&mut heap, 4); // at 13, 16, 19, 22
    assert_eq!(top(&heap), Obj(22));
    heap.collect();
    assert_eq!(top(&heap), Obj(10)); // the last pair made is still the last
    assert_eq!(elements(&heap, 0), (0..4).rev().map(Value::int).collect::<Vec<_>>());
    assert_eq!(heap.touched(), 12);
    assert_eq!(heap.counts().walked, 24); // three passes over eight objects
    assert_eq!(heap.counts().moved_words, 12);
    heap.collect(); // nothing to slide
    assert_eq!(heap.counts().moved_words, 12);
}

#[test]
fn a_copy_moves_every_object_and_lays_a_list_out_in_order() {
    let mut heap = Semispace::new(WORDS);
    list(&mut heap, 4); // garbage
    heap.pop();
    list(&mut heap, 4); // the list's first pair is the last one made, at word 21
    assert_eq!(top(&heap), Obj(21));
    heap.collect();
    // The copies are in the other half, in the order the scan met them: first pair first.
    let half = (WORDS / 2) as u32;
    assert_eq!(top(&heap), Obj(half));
    assert_eq!(heap.field(top(&heap), 1), Value::obj(Obj(half + 3)));
    assert_eq!(heap.used(), 12);
    assert_eq!((heap.counts().traced, heap.counts().walked, heap.counts().moved_words), (4, 0, 12));
    heap.collect(); // and back
    assert_eq!(top(&heap), Obj(0));
}

#[test]
fn an_object_reached_twice_is_copied_once() {
    let mut heap = Semispace::new(WORDS);
    list(&mut heap, 1);
    heap.push(heap.top());
    heap.alloc(Kind::Pair).unwrap(); // (x . x)
    heap.collect();
    let pair = top(&heap);
    assert_eq!(heap.field(pair, 0), heap.field(pair, 1));
    assert_eq!(heap.counts().traced, 2);
}

/// Push a list of `n` lists of three integers.
fn list_of_lists<H: Heap>(heap: &mut H, n: usize) {
    heap.push(Value::NIL);
    let slot = heap.roots() - 1;
    for _ in 0..n {
        list(heap, 3);
        heap.push(heap.root(slot));
        heap.alloc(Kind::Pair).unwrap();
        heap.set_root(slot, heap.top());
        heap.pop();
    }
}

#[test]
fn the_mark_stack_holds_one_entry_for_a_list_of_integers_however_long() {
    let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK);
    list(&mut heap, 10_000);
    heap.collect();
    assert_eq!((heap.counts().traced, heap.counts().deepest, heap.counts().overflows), (10_000, 1, 0));
}

#[test]
fn the_mark_stack_holds_an_entry_for_each_element_of_a_list_of_objects() {
    // A pair's two fields are both pushed before either is read, and the `cdr`, pushed
    // last, is read first: every element waits on the stack until the list has been walked.
    let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK);
    list_of_lists(&mut heap, 500);
    heap.collect();
    assert_eq!((heap.counts().traced, heap.counts().deepest, heap.counts().overflows), (2000, 500, 0));
}

#[test]
fn a_mark_stack_that_overflows_is_made_up_for_by_searching_the_heap() {
    for limit in [0, 1, 2, 16, 100] {
        let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, limit);
        list_of_lists(&mut heap, 500);
        list(&mut heap, 50); // garbage
        heap.pop();
        heap.collect();
        let counts = heap.counts();
        assert_eq!((counts.traced, heap.stats().live()), (2000, 2000), "a mark stack of {limit}");
        assert!(counts.overflows > 0 && counts.rescans > 0 && counts.deepest <= limit, "a mark stack of {limit}");
        assert_eq!(reachable(&heap), (2000, 6000));
    }
}

#[test]
fn marking_can_be_done_a_few_objects_at_a_time() {
    let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK);
    list(&mut heap, 100);
    let first = top(&heap);
    heap.mark_roots();
    assert!(heap.is_marked(first));
    let mut steps = 0;
    while heap.mark_some(7) {
        steps += 1;
    }
    assert_eq!(steps, 100 / 7);
    let mut pair = first;
    while let Some(next) = heap.field(pair, 1).as_obj() {
        assert!(heap.is_marked(next));
        pair = next;
    }
}

// ---- what a missed root does -----------------------------------------------------------

#[test]
#[should_panic(expected = "a freed object was used")]
fn a_swept_object_is_caught_until_its_words_are_allocated_again() {
    let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK);
    list(&mut heap, 1);
    let borrowed = top(&heap);
    heap.pop();
    heap.collect();
    heap.field(borrowed, 0);
}

#[test]
fn after_that_a_borrowed_reference_names_another_object() {
    let mut heap = MarkHeap::new(WORDS, Reclaim::Sweep, MARK_STACK);
    list(&mut heap, 1);
    let borrowed = top(&heap);
    assert_eq!(heap.field(borrowed, 0), Value::int(0));
    heap.pop();
    heap.collect();
    heap.push(Value::int(41));
    heap.push(Value::int(42));
    heap.alloc(Kind::Pair).unwrap();
    assert_eq!(heap.field(borrowed, 0), Value::int(41)); // no panic, and not the pair it was
}

#[test]
#[should_panic(expected = "a stale reference")]
fn a_reference_kept_across_a_copy_is_caught() {
    let mut heap = Semispace::new(WORDS);
    list(&mut heap, 1);
    let borrowed = top(&heap); // the object is on the root stack, and will be kept
    heap.collect();
    heap.field(borrowed, 0);
}

#[test]
fn until_the_old_half_is_in_use_again() {
    let mut heap = Semispace::new(WORDS);
    list(&mut heap, 2);
    let borrowed = top(&heap); // word 3: the list's first pair
    assert_eq!(heap.field(borrowed, 0), Value::int(1));
    heap.collect();
    heap.collect(); // back in the first half, where the first pair is now at word 0
    assert_eq!(heap.field(borrowed, 0), Value::int(0)); // and word 3 is the second pair
}
