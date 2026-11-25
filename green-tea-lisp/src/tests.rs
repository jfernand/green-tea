use crate::programs::PROGRAMS;
use crate::rc::{Cycles, Freeing, RcHeap};
use crate::{Error, Heap, Kind, Lisp, Obj, Value};
use std::string::String;
use std::vec::Vec;

const WORDS: usize = 1 << 16;
const MODES: [Freeing; 3] = [Freeing::Recursive, Freeing::Queued, Freeing::Lazy];

fn lisp() -> Lisp<RcHeap> {
    Lisp::new(RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak))
}

fn value(source: &str) -> String {
    lisp().run(source).unwrap()
}

/// The object on top of the root stack.
fn top(heap: &RcHeap) -> Obj {
    heap.top().as_obj().unwrap()
}

/// Push a list of `n` integers.
fn list(heap: &mut RcHeap, n: i64) {
    heap.push(Value::NIL);
    for i in 0..n {
        heap.push(Value::int(i));
        let cdr = heap.root(heap.roots() - 2);
        heap.push(cdr);
        heap.alloc(Kind::Pair).unwrap();
        heap.set_root(heap.roots() - 2, heap.top());
        heap.pop();
    }
}

// ---- the language -------------------------------------------------------------------

#[test]
fn atoms_evaluate_to_themselves() {
    assert_eq!(value("42"), "42");
    assert_eq!(value("-7"), "-7");
    assert_eq!(value("nil"), "nil");
    assert_eq!(value("()"), "nil");
    assert_eq!(value("#t"), "#t");
}

#[test]
fn repeated_direct_evaluation_errors_restore_roots_and_depth() {
    let mut lisp = lisp();
    let missing = lisp.intern("missing");
    let environment = lisp.heap.root(0);
    let caller_root = Value::int(99);
    lisp.heap.push(caller_root);

    for _ in 0..2 {
        lisp.heap.push(Value::sym(missing));
        lisp.heap.push(environment);
        assert_eq!(lisp.eval(), Err(Error::Unbound(missing)));
        assert_eq!(lisp.heap.roots(), 4);
        assert_eq!(lisp.heap.root(1), caller_root);
        lisp.heap.pop();
        lisp.heap.pop();
    }
    assert_eq!(lisp.deepest(), 1);

    lisp.heap.push(Value::int(42));
    lisp.heap.push(environment);
    assert_eq!(lisp.eval(), Ok(()));
    assert_eq!(lisp.heap.roots(), 3);
    assert_eq!(lisp.heap.root(1), caller_root);
    assert_eq!(lisp.heap.root(2), Value::int(42));
}

#[test]
fn quote_returns_its_argument_unevaluated() {
    assert_eq!(value("(quote (a b (c 1) nil))"), "(a b (c 1) nil)");
    assert_eq!(value("'(1 . 2)"), "(1 . 2)");
    assert_eq!(value("'x"), "x");
    assert_eq!(value("''x"), "(quote x)");
}

#[test]
fn pairs() {
    assert_eq!(value("(cons 1 2)"), "(1 . 2)");
    assert_eq!(value("(cons 1 (cons 2 nil))"), "(1 2)");
    assert_eq!(value("(car '(1 2 3))"), "1");
    assert_eq!(value("(cdr '(1 2 3))"), "(2 3)");
    assert_eq!(value("(null? (cdr '(1)))"), "#t");
    assert_eq!(value("(define p (cons 1 2)) (set-car! p 5) (set-cdr! p 6) p"), "(5 . 6)");
}

#[test]
fn arithmetic_and_comparison() {
    assert_eq!(value("(+ 2 3)"), "5");
    assert_eq!(value("(- 2 3)"), "-1");
    assert_eq!(value("(* 6 7)"), "42");
    assert_eq!(value("(remainder 17 5)"), "2");
    assert_eq!(value("(< 1 2)"), "#t");
    assert_eq!(value("(< 2 1)"), "nil");
    assert_eq!(value("(= 3 3)"), "#t");
    assert_eq!(value("(eq? 'a 'a)"), "#t");
    assert_eq!(value("(eq? 'a 'b)"), "nil");
    assert_eq!(value("(eq? (cons 1 2) (cons 1 2))"), "nil");
    assert_eq!(value("(define p (cons 1 2)) (eq? p p)"), "#t");
}

#[test]
fn if_takes_one_branch() {
    assert_eq!(value("(if #t 1 2)"), "1");
    assert_eq!(value("(if nil 1 2)"), "2");
    assert_eq!(value("(if 0 1 2)"), "1"); // only nil is false
    assert_eq!(value("(if nil 1)"), "nil");
    assert_eq!(value("(if #t 1 (car nil))"), "1");
}

#[test]
fn define_and_lambda() {
    assert_eq!(value("(define x 5) (+ x 1)"), "6");
    assert_eq!(value("(define x 5) (define x 6) x"), "6");
    assert_eq!(value("((lambda (a b) (- a b)) 5 3)"), "2");
    assert_eq!(value("(define (square x) (* x x)) (square 7)"), "49");
    assert_eq!(value("(define (f) 1 2 3) (f)"), "3");
    assert_eq!(value("(define f (lambda (x) x)) f"), "#<function>");
    assert_eq!(value("car"), "#<primitive car>");
    assert_eq!(value("(define car cdr) (car '(1 2))"), "(2)");
}

#[test]
fn closures_keep_their_environment() {
    assert_eq!(value("(define (adder n) (lambda (x) (+ x n))) (define add3 (adder 3)) (add3 4)"), "7");
    assert_eq!(value("(define n 100) (define (adder n) (lambda (x) (+ x n))) ((adder 1) 1)"), "2");
    // Lexical, not dynamic: `f` sees the global `n`, whatever its caller calls `n`.
    assert_eq!(value("(define n 1) (define (f) n) (define (g n) (f)) (g 2)"), "1");
}

#[test]
fn a_tail_call_needs_no_root_stack() {
    // 100,000 iterations, where 32 nested calls are all `eval` allows.
    assert_eq!(value("(define (loop n acc) (if (= n 0) acc (loop (- n 1) (+ acc 1)))) (loop 100000 0)"), "100000");
}

#[test]
fn an_integer_that_does_not_fit_in_61_bits_is_a_reader_error() {
    let too_large = Error::Syntax("an integer outside the 61 bits of a value");
    assert_eq!(value("1152921504606846975"), "1152921504606846975"); // 2^60 - 1
    assert_eq!(value("-1152921504606846976"), "-1152921504606846976"); // -2^60
    assert_eq!(lisp().run("1152921504606846976"), Err(too_large)); // 2^60
    assert_eq!(lisp().run("-1152921504606846977"), Err(too_large));
    assert_eq!(lisp().run("99999999999999999999"), Err(too_large)); // beyond an i64, too
    assert_eq!(lisp().run("-99999999999999999999"), Err(too_large));
    assert_eq!(value("'1+"), "1+"); // a symbol that starts with a digit is still one
}

#[test]
fn an_if_needs_a_test_and_a_branch() {
    let short = Error::Syntax("a form is too short");
    assert_eq!(lisp().run("(if #t)"), Err(short));
    assert_eq!(lisp().run("(if nil)"), Err(short));
    assert_eq!(lisp().run("(if)"), Err(short));
    assert_eq!(value("(if nil 1)"), "nil"); // the second branch may be left out
    assert_eq!(value("(if #t 1)"), "1");
}

#[test]
fn a_pair_reads_back_as_it_is_printed() {
    assert_eq!(value("(car '(1 . 2))"), "1");
    assert_eq!(value("(cdr '(1 . 2))"), "2");
    assert_eq!(value("(cdr '(1 2 . 3))"), "(2 . 3)");
    assert_eq!(value("'(1 . (2 3))"), "(1 2 3)");
    assert_eq!(value("(cdr '((1 . 2) . (3 . 4)))"), "(3 . 4)");
    assert_eq!(lisp().run("'( . 2)"), Err(Error::Syntax("a . with no element before it")));
    assert_eq!(lisp().run("'(1 .)"), Err(Error::Syntax("a . with nothing after it")));
    assert_eq!(lisp().run("'(1 . 2 3)"), Err(Error::Syntax("more than one value after a .")));
    assert_eq!(value("'(a .b ...)"), "(a .b ...)"); // symbols that only start with a dot
}

#[test]
fn errors() {
    let run = |source| lisp().run(source).unwrap_err();
    assert_eq!(run("(car 1)"), Error::NotAPair);
    assert_eq!(run("(+ 1 'a)"), Error::NotANumber);
    assert_eq!(run("(1 2)"), Error::NotAFunction);
    assert_eq!(run("(car)"), Error::Arguments);
    assert_eq!(run("((lambda (x) x))"), Error::Arguments);
    assert_eq!(run("((lambda (x) x) 1 2)"), Error::Arguments);
    assert_eq!(run("(remainder 1 0)"), Error::DivisionByZero);
    assert_eq!(run("(define (f n) (+ 1 (f n))) (f 1)"), Error::TooDeep);
    assert!(matches!(run("(car '(1)"), Error::Syntax(_)));
    assert!(matches!(run(")"), Error::Syntax(_)));
    assert!(matches!(run("(quote)"), Error::Syntax(_)));
    let mut lisp = lisp();
    assert_eq!(lisp.run("(+ 1 nope)"), Err(Error::Unbound(lisp.intern("nope"))));
}

#[test]
fn an_error_leaves_the_interpreter_usable_and_leaks_nothing() {
    let mut lisp = lisp();
    assert_eq!(lisp.run("(define x '(1 2 3)) (cons x (cons x (car 1)))"), Err(Error::NotAPair));
    assert_eq!(lisp.heap.roots(), 1);
    assert_eq!(lisp.run("x").unwrap(), "(1 2 3)");
    lisp.forget_globals();
    assert_eq!(lisp.heap.stats().live(), 0);
    lisp.heap.audit().unwrap();
}

#[test]
fn a_full_heap_is_an_error() {
    let mut lisp = Lisp::new(RcHeap::new(200, Freeing::Recursive, Cycles::Leak));
    let source = "(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc)))) (iota 100 nil)";
    assert_eq!(lisp.run(source), Err(Error::HeapFull));
    lisp.heap.audit().unwrap();
    assert_eq!(lisp.run("(iota 5 nil)").unwrap(), "(1 2 3 4 5)");
}

#[test]
fn printing_a_cycle_ends() {
    let printed = value("(define p (cons 1 nil)) (set-cdr! p p) p");
    assert!(printed.starts_with("(1 1 1") && printed.ends_with("...)"), "{printed}");
}

// ---- the heap -----------------------------------------------------------------------

#[test]
fn a_count_is_the_number_of_references() {
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak);
    heap.push(Value::int(1));
    heap.push(Value::int(2));
    heap.alloc(Kind::Pair).unwrap();
    let pair = top(&heap);
    assert_eq!(heap.count(pair), 1); // the root slot `alloc` pushed it to
    heap.push(Value::obj(pair));
    assert_eq!(heap.count(pair), 2);
    heap.push(Value::NIL);
    heap.alloc(Kind::Pair).unwrap(); // (pair): the reference moved from a root slot to a field
    assert_eq!(heap.count(pair), 2);
    let outer = top(&heap);
    heap.set_field(outer, 1, Value::obj(pair));
    assert_eq!(heap.count(pair), 3);
    heap.set_field(outer, 0, Value::NIL);
    assert_eq!(heap.count(pair), 2);
    heap.audit().unwrap();
    heap.pop(); // outer dies, and drops its reference
    assert_eq!(heap.count(pair), 1);
    assert_eq!((heap.stats().allocated, heap.stats().freed), (2, 1));
    heap.pop();
    assert_eq!(heap.stats().live(), 0);
    assert_eq!(heap.stats().live_words, 0);
    heap.audit().unwrap();
}

#[test]
fn alloc_moves_its_fields_without_counting() {
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak);
    list(&mut heap, 1);
    let before = heap.counts();
    heap.push(Value::NIL);
    heap.alloc(Kind::Pair).unwrap();
    assert_eq!(heap.counts(), before);
}

#[test]
fn freed_objects_are_reused_before_fresh_words() {
    for freeing in MODES {
        let mut heap = RcHeap::new(WORDS, freeing, Cycles::Leak);
        for _ in 0..100 {
            list(&mut heap, 10);
            heap.pop();
        }
        heap.finish_freeing();
        assert_eq!(heap.stats().allocated, 1000);
        assert_eq!(heap.stats().live(), 0);
        // Ten pairs of three words, over and over. A lazy heap frees two objects before
        // each allocation, which is enough to stay ahead.
        assert_eq!(heap.touched(), 30, "{freeing:?}");
        heap.audit().unwrap();
    }
}

#[test]
fn storing_a_value_over_itself_keeps_it() {
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak);
    list(&mut heap, 1);
    let slot = heap.roots() - 1;
    heap.set_root(slot, heap.root(slot)); // the count goes 1, 2, 1: never 0
    assert_eq!(heap.count(top(&heap)), 1);
    heap.push(heap.top());
    heap.push(Value::NIL);
    heap.alloc(Kind::Pair).unwrap();
    let outer = top(&heap);
    heap.set_root(slot, Value::NIL); // now the list is held by `outer` alone
    heap.set_field(outer, 0, heap.field(outer, 0));
    assert_eq!(heap.stats().freed, 0);
    heap.audit().unwrap();
}

#[test]
fn storing_what_only_the_old_value_holds_keeps_it() {
    // slot = (cdr slot): the new value's only reference is in the old one.
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak);
    list(&mut heap, 3);
    let slot = heap.roots() - 1;
    heap.set_root(slot, heap.field(top(&heap), 1));
    assert_eq!(heap.stats().freed, 1);
    assert_eq!(heap.field(top(&heap), 0), Value::int(1));
    heap.audit().unwrap();
}

#[test]
#[should_panic(expected = "a freed object was used")]
fn a_borrowed_reference_dies_with_the_last_counted_one() {
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak);
    list(&mut heap, 1);
    let borrowed = top(&heap);
    heap.pop();
    heap.field(borrowed, 0);
}

#[test]
fn freeing_a_list_recurses_once_per_pair_unless_queued() {
    for (freeing, deepest) in [(Freeing::Recursive, 1000), (Freeing::Queued, 0), (Freeing::Lazy, 0)] {
        let mut heap = RcHeap::new(WORDS, freeing, Cycles::Leak);
        list(&mut heap, 1000);
        heap.pop();
        assert_eq!(heap.counts().deepest, deepest, "{freeing:?}");
        if freeing == Freeing::Lazy {
            assert_eq!(heap.stats().freed, 0);
            assert_eq!(heap.counts().largest_burst, 0);
            heap.push(Value::NIL);
            heap.push(Value::NIL);
            heap.alloc(Kind::Pair).unwrap();
            assert_eq!(heap.stats().freed, 2);
            heap.pop();
            heap.finish_freeing();
        } else {
            assert_eq!(heap.counts().largest_burst, 1000);
        }
        assert_eq!(heap.stats().freed, heap.stats().allocated);
        heap.audit().unwrap();
    }
}

#[test]
fn a_lazy_heap_finishes_freeing_before_it_reports_full() {
    let mut heap = RcHeap::new(30, Freeing::Lazy, Cycles::Leak);
    list(&mut heap, 10);
    heap.pop();
    list(&mut heap, 10);
    heap.pop();
    heap.finish_freeing();
    heap.audit().unwrap();
}

// ---- cycles -------------------------------------------------------------------------

/// Push a ring of `n` pairs: a list whose last pair's `cdr` is the first pair.
fn ring(heap: &mut RcHeap, n: i64) {
    list(heap, n);
    let first = top(heap);
    let mut last = first;
    while let Some(next) = heap.field(last, 1).as_obj() {
        last = next;
    }
    heap.set_field(last, 1, Value::obj(first));
}

#[test]
fn a_ring_that_nothing_refers_to_is_never_freed() {
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Leak);
    ring(&mut heap, 8);
    heap.pop();
    assert_eq!(heap.stats().freed, 0);
    assert_eq!(heap.reachable(), (0, 8));
    heap.audit().unwrap(); // every count is right: each pair is referred to once
}

#[test]
fn trial_deletion_frees_a_ring() {
    for freeing in MODES {
        let mut heap = RcHeap::new(WORDS, freeing, Cycles::Collect { every: usize::MAX });
        ring(&mut heap, 8);
        heap.pop();
        assert_eq!(heap.stats().freed, 0);
        assert_eq!(heap.collect_cycles(), 8);
        assert_eq!(heap.stats().live(), 0);
        assert_eq!(heap.counts().trial_decrements, 8);
        heap.audit().unwrap();
    }
}

#[test]
fn trial_deletion_keeps_a_ring_that_is_referred_to() {
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Collect { every: usize::MAX });
    ring(&mut heap, 8);
    heap.push(heap.field(top(&heap), 1)); // a second reference, into the middle
    heap.set_root(heap.roots() - 2, Value::NIL); // drop the first: the ring is a candidate
    assert_eq!(heap.collect_cycles(), 0);
    assert_eq!(heap.reachable(), (8, 0));
    heap.audit().unwrap();
    heap.pop();
    assert_eq!(heap.collect_cycles(), 8);
    heap.audit().unwrap();
}

#[test]
fn trial_deletion_frees_a_ring_and_spares_what_it_refers_to() {
    let mut heap = RcHeap::new(WORDS, Freeing::Recursive, Cycles::Collect { every: usize::MAX });
    list(&mut heap, 3); // shared: held by a root and by the ring
    ring(&mut heap, 4);
    heap.set_field(top(&heap), 0, heap.root(heap.roots() - 2));
    heap.pop();
    assert_eq!(heap.collect_cycles(), 4);
    assert_eq!(heap.reachable(), (3, 0));
    assert_eq!(heap.count(top(&heap)), 1);
    heap.audit().unwrap();
}

#[test]
fn a_full_heap_searches_for_cycles_before_it_gives_up() {
    let mut heap = RcHeap::new(100, Freeing::Recursive, Cycles::Collect { every: usize::MAX });
    for _ in 0..50 {
        ring(&mut heap, 8); // 24 words each: the fifth would not fit
        heap.pop();
    }
    assert!(heap.counts().searches >= 10);
    heap.audit().unwrap();
}

/// A small generator: the same sequence on every run.
struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n
    }
}

/// A random program against the interface: it allocates, overwrites fields with whatever
/// it holds (which makes cycles), and drops roots. After every search for cycles, what is
/// allocated must be exactly what the root stack reaches.
#[test]
fn after_a_search_everything_allocated_is_reachable() {
    for freeing in MODES {
        for seed in 0..20 {
            let mut random = Random(seed);
            let mut heap = RcHeap::new(WORDS, freeing, Cycles::Collect { every: 1 + random.below(40) });
            for step in 0..3000 {
                let roots = heap.roots();
                let held = |heap: &RcHeap, random: &mut Random| {
                    if roots == 0 || random.below(4) == 0 { Value::int(7) } else { heap.root(random.below(roots)) }
                };
                match random.below(6) {
                    0 | 1 => {
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
                if step % 500 == 499 {
                    heap.collect_cycles();
                    heap.audit().unwrap_or_else(|e| panic!("{freeing:?}, seed {seed}, step {step}: {e}"));
                    assert_eq!(heap.reachable().1, 0, "{freeing:?}, seed {seed}, step {step}");
                }
            }
            while heap.roots() > 0 {
                heap.pop();
            }
            heap.collect_cycles();
            heap.audit().unwrap();
            assert_eq!(heap.stats().live(), 0, "{freeing:?}, seed {seed}");
        }
    }
}

// ---- the programs -------------------------------------------------------------------

#[test]
fn every_program_computes_its_value_on_every_heap() {
    for program in &PROGRAMS {
        for freeing in MODES {
            for cycles in [Cycles::Leak, Cycles::Collect { every: 256 }] {
                let mut lisp = Lisp::new(RcHeap::new(WORDS, freeing, cycles));
                assert_eq!(lisp.run(program.source).unwrap(), program.value, "{} {freeing:?} {cycles:?}", program.name);
                lisp.forget_globals();
                lisp.heap.finish_freeing();
                if cycles != Cycles::Leak {
                    lisp.heap.collect_cycles();
                }
                lisp.heap.audit().unwrap();
                let leaked = lisp.heap.stats().live();
                assert_eq!(leaked == 0, !program.cyclic || cycles != Cycles::Leak, "{}: {leaked} leaked", program.name);
                assert_eq!(lisp.heap.reachable(), (0, leaked));
            }
        }
    }
}

#[test]
fn the_programs_leak_what_their_cycles_hold() {
    let leaked: Vec<u64> = PROGRAMS
        .iter()
        .map(|program| {
            let mut lisp = lisp();
            lisp.run(program.source).unwrap();
            lisp.forget_globals();
            lisp.heap.stats().live()
        })
        .collect();
    // 100 rings of 8 pairs. 100 cycles of a pair, a closure and a binding, and the 21 pairs
    // of `make-adder`'s source that the leaked closures refer to as their parameters and body.
    assert_eq!(leaked, [0, 0, 0, 800, 321]);
}

#[test]
fn the_freeing_policy_changes_no_count() {
    for program in &PROGRAMS {
        let counts: Vec<_> = MODES
            .iter()
            .map(|&freeing| {
                let mut lisp = Lisp::new(RcHeap::new(WORDS, freeing, Cycles::Leak));
                lisp.run(program.source).unwrap();
                lisp.forget_globals();
                lisp.heap.finish_freeing();
                let (stats, counts) = (lisp.heap.stats(), lisp.heap.counts());
                (stats.allocated, stats.freed, counts.increments(), counts.decrements())
            })
            .collect();
        assert_eq!(counts[0], counts[1], "{}", program.name);
        assert_eq!(counts[0], counts[2], "{}", program.name);
    }
}

#[test]
fn the_programs_nest_fewer_calls_than_eval_allows() {
    let deepest: Vec<usize> = PROGRAMS
        .iter()
        .map(|program| {
            let mut lisp = lisp();
            lisp.run(program.source).unwrap();
            lisp.deepest()
        })
        .collect();
    assert_eq!(deepest, [7, 17, 22, 9, 6]);
}

#[test]
fn a_loop_over_a_list_frees_each_pair_as_it_passes() {
    // `list` makes a list of 100 pairs and reverses it into another. If the first were
    // kept until the second was finished, 600 words of lists would be live at once.
    let source = PROGRAMS[0].source;
    let definitions = source.rsplit_once("(rounds").unwrap().0;
    let mut lisp = lisp();
    lisp.run(definitions).unwrap();
    let text = lisp.heap.stats().live_words;
    lisp.run("(rounds 10 0)").unwrap();
    let lists = lisp.heap.stats().peak_words - text;
    assert!((300..400).contains(&lists), "{lists} words beyond the program's text");
}
