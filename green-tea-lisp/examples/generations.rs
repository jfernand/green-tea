//! The generational heap of green-tea-lisp, measured on the host:
//!
//!     cargo run --release -p green-tea-lisp --example generations
//!
//! How many objects of each program are alive at the first collection after they were
//! made, for nurseries of growing size; the collections of each program in 4,096 words,
//! beside the heaps of `heaps.rs`; what the write barrier did; the smallest heap each
//! program runs in; and, with a clock, what a store costs with and without a barrier, and
//! a minor collection beside a full one. The counts are the same on every run. The times
//! are not, and the lines that hold them say so.

use green_tea_lisp::gc::Collect;
use green_tea_lisp::generational::{Barrier, GenHeap, Generations, MUTATING};
use green_tea_lisp::mark::{MarkHeap, Reclaim};
use green_tea_lisp::programs::{PROGRAMS, Program};
use green_tea_lisp::semispace::Semispace;
use green_tea_lisp::{Heap, Kind, Lisp, Obj, Stats, Value};
use std::hint::black_box;
use std::sync::OnceLock;
use std::time::Instant;

/// Room for everything any of the programs allocates: no major collection happens.
const ROOMY: usize = 1 << 20;
const MEDIUM: usize = 4096;
const MARK_STACK: usize = 1024;

fn all() -> impl Iterator<Item = &'static Program> {
    PROGRAMS.iter().chain(&MUTATING)
}

/// Run a program, and return what the heap counted as the program left it.
fn run(heap: GenHeap, program: &Program) -> (Lisp<GenHeap>, Stats, Generations) {
    let mut lisp = Lisp::new(heap);
    assert_eq!(lisp.run(program.source).as_deref(), Ok(program.value), "{}", program.name);
    let (stats, generations) = (lisp.heap.stats(), lisp.heap.generations());
    (lisp, stats, generations)
}

fn nanoseconds() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Push a list of `n` integers.
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

fn lifetimes() {
    const NURSERIES: [usize; 8] = [32, 64, 128, 256, 512, 1024, 2048, 4096];
    println!("objects copied out of the nursery, in a heap of {ROOMY} words, by the size of the nursery in words:");
    print!("  program   allocated");
    for nursery in NURSERIES {
        print!(" {nursery:6}");
    }
    println!();
    let mut shares = Vec::new();
    for program in all() {
        let mut share = format!("  {:8}           ", program.name);
        let mut allocated = 0;
        let mut row = String::new();
        for nursery in NURSERIES {
            let (_, stats, generations) = run(GenHeap::new(ROOMY, nursery, Barrier::Filter), program);
            assert_eq!(generations.major, 0);
            allocated = stats.allocated;
            row.push_str(&format!(" {:6}", generations.promoted));
            let tenths = (generations.promoted * 1000 + stats.allocated / 2) / stats.allocated;
            share.push_str(&format!(" {:4}.{}", tenths / 10, tenths % 10));
        }
        println!("  {:8} {allocated:10}{row}", program.name);
        shares.push(share);
    }
    println!("the same, as a percentage of the objects allocated:");
    for share in shares {
        println!("{share}");
    }
    println!("minor collections in those runs, and the most objects one of them copied:");
    for program in all() {
        print!("  {:8}           ", program.name);
        for nursery in NURSERIES {
            let (_, _, generations) = run(GenHeap::new(ROOMY, nursery, Barrier::Filter), program);
            print!(" {:6}", format!("{}/{}", generations.minor, generations.largest_minor));
        }
        println!();
    }
}

fn collections() {
    println!("five programs in {MEDIUM} words: a heap that copies, one that compacts, and two generations:");
    println!(
        "  program  heap             collections  minor  major  copied  marked  traced  most copied  most marked  walked  words moved"
    );
    for program in &PROGRAMS {
        for (name, counts) in [
            ("copying", {
                let mut lisp = Lisp::new(Semispace::new(MEDIUM));
                lisp.run(program.source).unwrap();
                lisp.heap.counts()
            }),
            ("mark-compact", {
                let mut lisp = Lisp::new(MarkHeap::new(MEDIUM, Reclaim::Compact, MARK_STACK));
                lisp.run(program.source).unwrap();
                lisp.heap.counts()
            }),
        ] {
            println!(
                "  {:8} {name:16} {:11} {:>6} {:>6} {:>7} {:>7} {:7} {:>12} {:>12} {:7} {:12}",
                program.name,
                counts.collections,
                "",
                "",
                "",
                "",
                counts.traced,
                "",
                "",
                counts.walked,
                counts.moved_words
            );
        }
        for nursery in [128, 256, 512, 1024, usize::MAX] {
            let name =
                if nursery == usize::MAX { String::from("nursery of half") } else { format!("nursery of {nursery}") };
            let (lisp, _, generations) = run(GenHeap::new(MEDIUM, nursery, Barrier::Filter), program);
            let counts = lisp.heap.counts();
            assert_eq!(counts.traced, generations.promoted + generations.marked);
            println!(
                "  {:8} {name:16} {:11} {:6} {:6} {:7} {:7} {:7} {:12} {:12} {:7} {:12}",
                program.name,
                counts.collections,
                generations.minor,
                generations.major,
                generations.promoted,
                generations.marked,
                counts.traced,
                generations.largest_minor,
                generations.largest_major,
                counts.walked,
                counts.moved_words
            );
        }
    }
}

fn barrier() {
    println!("the write barrier, in {MEDIUM} words with a nursery of 256:");
    println!(
        "  program   allocated  stores  into old  old to young  recorded  most remembered  objects scanned  references found  recorded by the barrier that tests nothing"
    );
    for program in all() {
        let (_, stats, filter) = run(GenHeap::new(MEDIUM, 256, Barrier::Filter), program);
        let (_, _, every) = run(GenHeap::new(MEDIUM, 256, Barrier::Every), program);
        println!(
            "  {:8} {:10} {:7} {:9} {:13} {:9} {:16} {:16} {:17} {:10}",
            program.name,
            stats.allocated,
            filter.stores,
            filter.stores_old,
            filter.old_to_young,
            filter.recorded,
            filter.remembered_most,
            filter.remembered_scanned,
            filter.remembered_found,
            every.recorded
        );
    }
    println!("root slots read by the minor collections of those runs, and objects they copied:");
    for program in all() {
        let (_, _, g) = run(GenHeap::new(MEDIUM, 256, Barrier::Filter), program);
        println!(
            "  {:8} {:5} minor collections read {:6} root slots and {:4} remembered objects, and copied {:5} objects",
            program.name, g.minor, g.minor_roots, g.remembered_scanned, g.promoted
        );
    }
}

/// What became of the objects that were promoted: a full collection at the end of the
/// run, with the definitions still held, finds the ones that have died since.
fn promoted() {
    println!(
        "promoted objects, in {MEDIUM} words with a nursery of 256, and how many were dead when the program ended:"
    );
    println!("  program   allocated  promoted  dead at the end  reachable at the end");
    for program in all() {
        let (mut lisp, stats, _) = run(GenHeap::new(MEDIUM, 256, Barrier::Filter), program);
        lisp.heap.collect();
        let generations = lisp.heap.generations();
        println!(
            "  {:8} {:10} {:9} {:16} {:21}",
            program.name,
            stats.allocated,
            generations.promoted,
            generations.old_freed,
            lisp.heap.stats().live()
        );
        assert_eq!(generations.promoted - generations.old_freed, lisp.heap.stats().live());
    }
}

fn smallest() {
    println!("the smallest heap each program computes its value in, in words:");
    println!("  program   mark-compact  copying  two generations, nursery of 256 at most");
    for program in &PROGRAMS {
        let fits = |result: Result<String, green_tea_lisp::Error>| result.as_deref() == Ok(program.value);
        let first = |run: &dyn Fn(usize) -> bool| (1..).find(|&words| run(words)).unwrap();
        let compact = first(&|w| fits(Lisp::new(MarkHeap::new(w, Reclaim::Compact, MARK_STACK)).run(program.source)));
        let copying = first(&|w| fits(Lisp::new(Semispace::new(w)).run(program.source)));
        let two = first(&|w| fits(Lisp::new(GenHeap::new(w, 256, Barrier::Filter)).run(program.source)));
        println!("  {:8} {compact:13} {copying:8} {two:16}", program.name);
    }
}

#[inline(never)]
fn store<H: Heap>(heap: &mut H, obj: Obj, v: Value) {
    heap.set_field(obj, 0, v);
}

const STORES: u64 = 1_000_000;

/// A million stores into one object, timed.
fn stores<H: Heap>(heap: &mut H, obj: Obj, v: Value) -> u64 {
    let started = nanoseconds();
    for _ in 0..STORES {
        store(black_box(heap), black_box(obj), black_box(v));
    }
    nanoseconds() - started
}

fn times(run: usize) {
    println!("{STORES} stores into one pair, run {run}; the last two columns are times, and vary from run to run:");
    println!(
        "  heap                         the pair  the value            recorded  ns for each store  ns for the next minor collection"
    );
    let line = |name: &str, pair: &str, value: &str, recorded: u64, ns: u64, minor: Option<u64>| {
        let minor = minor.map_or(String::from("-"), |ns| ns.to_string());
        println!(
            "  {name:28} {pair:9} {value:20} {recorded:8} {:14}.{} {minor:>33}",
            ns / STORES,
            ns * 10 / STORES % 10
        );
    };
    // A heap with no barrier.
    let mut plain = Semispace::new(4096);
    list(&mut plain, 2);
    let pair = plain.top().as_obj().unwrap();
    let ns = stores(&mut plain, pair, Value::int(1));
    line("copying, no barrier", "", "an integer", 0, ns, None);
    for (name, barrier) in
        [("two generations, filter", Barrier::Filter), ("two generations, every store", Barrier::Every)]
    {
        for (old, value) in
            [(false, "an integer"), (true, "an integer"), (true, "an old reference"), (true, "a young reference")]
        {
            let mut heap = GenHeap::new(4096, 256, barrier);
            list(&mut heap, 2);
            if old {
                heap.minor();
            }
            let pair = heap.top().as_obj().unwrap();
            let v = match value {
                "an integer" => Value::int(1),
                "an old reference" => heap.field(pair, 1),
                _ => {
                    list(&mut heap, 1);
                    heap.top()
                }
            };
            let ns = stores(&mut heap, pair, v);
            let recorded = heap.generations().recorded;
            heap.set_clock(nanoseconds);
            heap.minor();
            let minor = heap.generations().minor_pause_longest;
            line(name, if old { "old" } else { "young" }, value, recorded, ns, Some(minor));
        }
    }
}

/// One collection with a list in the old space that is kept and a nursery full of garbage.
fn one_collection(run: usize) {
    println!(
        "one collection with a list that is kept and a nursery of 1024 words of garbage, run {run}; the last column is a time:"
    );
    println!("  kept objects  collection          traced   walked  words moved  nanoseconds");
    for kept in [1_000, 100_000] {
        let words = 8 * kept as usize + 4096;
        let mut heap = GenHeap::new(words, 1024, Barrier::Filter);
        list(&mut heap, kept);
        heap.collect();
        for (name, full) in [("minor", false), ("minor and major", true)] {
            list(&mut heap, 300);
            heap.pop();
            heap.set_clock(nanoseconds);
            let before = (heap.counts(), heap.generations());
            if full {
                heap.collect()
            } else {
                heap.minor()
            }
            let (counts, generations) = (heap.counts(), heap.generations());
            let pause = generations.minor_pause_total + generations.major_pause_total
                - before.1.minor_pause_total
                - before.1.major_pause_total;
            println!(
                "  {kept:12}  {name:16} {:9} {:8} {:12} {:12}",
                counts.traced - before.0.traced,
                counts.walked - before.0.walked,
                counts.moved_words - before.0.moved_words,
                pause
            );
        }
        let mut heap = Semispace::new(words);
        list(&mut heap, kept);
        list(&mut heap, 300);
        heap.pop();
        heap.set_clock(nanoseconds);
        heap.collect();
        let counts = heap.counts();
        println!(
            "  {kept:12}  {:16} {:9} {:8} {:12} {:12}",
            "copying", counts.traced, counts.walked, counts.moved_words, counts.pause_total
        );
    }
}

fn main() {
    lifetimes();
    collections();
    barrier();
    promoted();
    smallest();
    for run in 1..=3 {
        times(run);
    }
    for run in 1..=3 {
        one_collection(run);
    }
}
