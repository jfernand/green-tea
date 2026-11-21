//! What a store costs with a write barrier and without, and what a program costs on a
//! heap with two generations and on one that copies, in instructions. This program does
//! the work and nothing else, and is meant to be run under a tool that counts the
//! instructions a process executes:
//!
//!     valgrind --tool=callgrind target/release/examples/store_cost store <case> <count>
//!     valgrind --tool=callgrind target/release/examples/store_cost program <name> <heap> <runs>
//!
//! A run with a count of 0 is what the process costs without the work.

use green_tea_lisp::generational::{Barrier, GenHeap, MUTATING};
use green_tea_lisp::programs::PROGRAMS;
use green_tea_lisp::semispace::Semispace;
use green_tea_lisp::{Heap, Kind, Lisp, Obj, Value};
use std::hint::black_box;

/// Push a pair `(a . b)`.
fn pair<H: Heap>(heap: &mut H, a: i64, b: i64) -> Obj {
    heap.push(Value::int(a));
    heap.push(Value::int(b));
    heap.alloc(Kind::Pair).unwrap();
    heap.top().as_obj().unwrap()
}

#[inline(never)]
fn store<H: Heap>(heap: &mut H, obj: Obj, v: Value) {
    heap.set_field(obj, 0, v);
}

fn stores<H: Heap>(heap: &mut H, obj: Obj, v: Value, count: u64) {
    for _ in 0..count {
        store(black_box(heap), black_box(obj), black_box(v));
    }
}

/// `count` stores into the first field of one pair.
fn store_case(case: &str, count: u64) {
    if case == "plain" {
        let mut heap = Semispace::new(4096);
        let target = pair(&mut heap, 0, 0);
        return stores(&mut heap, target, Value::int(1), count);
    }
    let (barrier, case) = match case.strip_prefix("every-") {
        Some(rest) => (Barrier::Every, rest),
        None => (Barrier::Filter, case),
    };
    let mut heap = GenHeap::new(4096, 256, barrier);
    pair(&mut heap, 0, 0);
    pair(&mut heap, 0, 0);
    if case != "young" {
        heap.minor(); // both pairs are old now
    }
    let (target, other) = (heap.root(0).as_obj().unwrap(), heap.root(1));
    let value = match case {
        "young" | "old-integer" => Value::int(1),
        "old-old" => other,
        "old-young" => Value::obj(pair(&mut heap, 0, 0)),
        _ => panic!("no such case: {case}"),
    };
    stores(&mut heap, target, value, count);
}

fn program_case(name: &str, heap: &str, runs: u64) {
    let program = PROGRAMS.iter().chain(&MUTATING).find(|p| p.name == name).expect("no such program");
    for _ in 0..runs {
        let value = match heap {
            "copying" => Lisp::new(Semispace::new(4096)).run(program.source),
            "half" => Lisp::new(GenHeap::new(4096, usize::MAX, Barrier::Filter)).run(program.source),
            nursery => {
                let nursery = nursery.parse().expect("copying, half, or the words of a nursery");
                Lisp::new(GenHeap::new(4096, nursery, Barrier::Filter)).run(program.source)
            }
        };
        assert_eq!(black_box(value).as_deref(), Ok(program.value));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[1..] {
        ["store", case, count] => store_case(case, count.parse().unwrap()),
        ["program", name, heap, runs] => program_case(name, heap, runs.parse().unwrap()),
        _ => panic!("usage: store_cost store <case> <count> | store_cost program <name> <heap> <runs>"),
    }
}
