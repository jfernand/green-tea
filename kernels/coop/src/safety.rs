//! The heap-safety test, `cargo xtask kernel --kernel safety`.
//!
//! First the bug: two tasks share a heap, which is green-tea-fit's `Fit`, and one of them
//! writes to a block after freeing it. The other is handed memory that isn't free.
//!
//! Then the same allocator hardened, in green-tea-fit's `Hardened`, one defence at a
//! time: safe unlinking, encoded links, a canary, a quarantine, random choices. Six
//! mistakes are made against each of the six allocators, and the test prints which call
//! caught each, if any did. Then what the defences change about when and where a freed
//! block is used again, and what they cost: bytes in a block, and the footprint and the
//! searching of the `fit` test's random workload.

use crate::frag::random_trace;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use green_tea_bare::fit::hardened::{ALL, CANARY, ENCODE, Fault, Hardened, QUARANTINE, UNLINK};
use green_tea_bare::fit::trace::{self, Random};
use green_tea_bare::fit::{Fit, Order, Policy};
use green_tea_bare::Framebuffer;
use green_tea_bare::runtime::{run, spawn};
use green_tea_bare::screen::Screen;
use green_tea_bare::task::yield_now;
use green_tea_bare::{Global, println};

const WORD: usize = size_of::<usize>();
/// The region the allocators get, from the kernel's heap.
const REGION: usize = 1024 * 1024;
/// The seed of every hardened allocator here: the same secrets and the same choices on
/// every board, every run.
const SEED: u64 = 0x2545_f491_4f6c_dd1d;

/// The six allocators, each with one defence more than the one before.
const NONE: u8 = 0;
const L1: u8 = UNLINK;
const L2: u8 = L1 | ENCODE;
const L3: u8 = L2 | CANARY;
const L4: u8 = L3 | QUARANTINE;
const L5: u8 = ALL;
const NAMES: [&str; 6] = ["no defence", "+ safe unlinking", "+ encoded links", "+ canary", "+ quarantine", "+ random"];

/// Call a function that is generic over the defences once for each of the six.
macro_rules! each {
    ($f:ident($($arg:expr),*)) => {
        [$f::<NONE>($($arg),*), $f::<L1>($($arg),*), $f::<L2>($($arg),*), $f::<L3>($($arg),*), $f::<L4>($($arg),*),
         $f::<L5>($($arg),*)]
    };
}

pub fn test(fb: Framebuffer) {
    let mut memory = vec![0u128; REGION / 16];
    let base = memory.as_mut_ptr() as usize;
    let verdict = two_tasks(base)
        .and_then(|()| words(base))
        .and_then(|()| mistakes(base))
        .and_then(|()| reuse(base))
        .and_then(|delays| Ok((delays, costs(base)?)));
    match verdict {
        Err(e) => println!("FAIL: {e}"),
        Ok(((fixed, least, most), (plain, hardened))) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: a write to a freed block went unseen by the plain allocator and was caught by encoded links \
                 and by the quarantine; a freed block was reused after {fixed} allocations, or {least} to {most} at \
                 random; the workload's footprint went from {plain} to {hardened} bytes"
            );
        }
    }
}

fn put(addr: usize, word: usize, value: usize) {
    unsafe { ((addr + word * WORD) as *mut usize).write(value) }
}

fn get(addr: usize, word: usize) -> usize {
    unsafe { ((addr + word * WORD) as *const usize).read() }
}

fn dump(from: usize, words: usize) {
    for addr in (from..).step_by(WORD).take(words) {
        println!("  {addr:#x}  {:#018x}", get(addr, 0));
    }
}

/// Make something at `fake` look like a free block of 64 bytes whose back link is `from`.
fn forge(fake: usize, from: usize) {
    for (word, value) in [(0, 64), (1, 0), (2, from), (7, 64)] {
        put(fake, word, value);
    }
}

// ---- The bug ---------------------------------------------------------------------------

/// What the two tasks tell each other, and the test: the heap they share, the logger's
/// line and buffer, and the clerk's two records and what it read back from them.
static SHARED: Global<usize> = Global::new(0);
static LOGGER: Global<[usize; 2]> = Global::new([0; 2]);
static CLERK: Global<[usize; 4]> = Global::new([0; 4]);

fn with_shared<R>(f: impl FnOnce(&mut Fit) -> R) -> R {
    SHARED.with(|address| {
        assert_ne!(*address, 0, "the shared allocator hasn't been initialized");
        // SAFETY: `two_tasks` keeps the allocator alive through `run`; this closure never
        // yields, so cooperative tasks cannot hold a second mutable reference at once.
        f(unsafe { &mut *(*address as *mut Fit) })
    })
}

/// Keeps a line of text in a block of its own, and a buffer. Its bug: it frees the line
/// and goes on using it.
fn logger() {
    let line = with_shared(|fit| fit.allocate(48, 8).unwrap());
    let buffer = with_shared(|fit| fit.allocate(96, 8).unwrap());
    LOGGER.set([line, buffer]);
    println!("  logger: a line at {line:#x}, and a buffer of 96 bytes at {buffer:#x}");
    with_shared(|fit| unsafe { fit.free(line).unwrap() });
    // The write after the free. The line's first word is now where the allocator keeps
    // its link to the next free block, and what is written there is the address of the
    // buffer's second word. The buffer's contents are the worst they could be: they read
    // as a free block of 64 bytes.
    forge(buffer + WORD, line - WORD);
    put(line, 0, buffer + WORD);
    println!("  logger: frees the line, then writes {:#x} to its first word", buffer + WORD);
    yield_now();
    unsafe { core::ptr::write_bytes(buffer as *mut u8, 0x61, 96) };
    println!("  logger: fills its buffer with the letter a");
}

/// Keeps two records of 48 bytes, a number in the first word of each.
fn clerk() {
    let records = [1000, 2000].map(|amount| {
        let record = with_shared(|fit| fit.allocate(48, 8).unwrap());
        put(record, 0, amount);
        record
    });
    println!("  clerk:  a record at {:#x} holding 1000, and one at {:#x} holding 2000", records[0], records[1]);
    yield_now();
    let read = records.map(|record| get(record, 0));
    CLERK.set([records[0], records[1], read[0], read[1]]);
    println!("  clerk:  its records now hold {} and {:#x}", read[0], read[1]);
}

fn two_tasks(base: usize) -> Result<(), &'static str> {
    // SAFETY: the region is the test's, and this is the only allocator using it.
    let mut fit = unsafe { Fit::new(base, 4096, Policy::FirstFit, Order::Lifo) };
    SHARED.set(&raw mut fit as usize);
    println!("two tasks share a heap of {} bytes, first fit over a LIFO list:", fit.capacity());
    run(
        || {
            spawn(logger);
            spawn(clerk);
        },
        |_| {},
    );
    SHARED.set(0);
    let ([line, buffer], [first, second, kept, lost]) = (LOGGER.get(), CLERK.get());
    let inside = second >= buffer && second + 48 <= buffer + 96;
    println!("the clerk's second record is inside the logger's buffer: {inside}");
    println!(
        "the allocator returned no error; a third request for 48 bytes: {}; its check: {:?}",
        if fit.allocate(48, 8).is_some() { "granted" } else { "refused" },
        fit.check()
    );
    if first != line || !inside || kept != 1000 || lost != 0x6161_6161_6161_6161 {
        return Err("a write to a freed block should have given the clerk part of the logger's buffer");
    }
    Ok(())
}

// ---- What the defences put in a block ------------------------------------------------------

/// A block for 24 bytes filled with the letter a, and the same block after it is freed,
/// under four of the allocators.
fn words(base: usize) -> Result<(), &'static str> {
    fn block<const D: u8>(base: usize, what: &str) -> [usize; 6] {
        // SAFETY: as in `two_tasks`.
        let mut heap = unsafe { Hardened::<D>::new(base, 4096, SEED) };
        let [a, _b] = [0; 2].map(|_| heap.allocate(24, 8).unwrap().unwrap());
        unsafe { core::ptr::write_bytes(a as *mut u8, 0x61, 24) };
        let allocated = core::array::from_fn(|word| get(a - WORD, word));
        if D == L3 {
            println!("a block for 24 bytes, with a canary:");
            dump(a - WORD, 6);
        }
        unsafe { heap.free(a).unwrap() };
        println!("{what}");
        dump(a - WORD, 6);
        allocated
    }
    let plain = block::<NONE>(base, "a block for 24 bytes, freed, with no defence:");
    block::<L2>(base, "freed, with encoded links:");
    let canary = block::<L3>(base, "freed, with a canary and encoded links:");
    block::<L4>(base, "freed, into the quarantine:");
    // Only the fifth word differs: padding without a canary, the canary with one.
    if plain[..4] != canary[..4] || plain[5] != 0x31 || canary[5] != 0x31 || canary[4] == plain[4] {
        return Err("a canary should fit in the padding of a block for 24 bytes");
    }
    if get(base + 2 * WORD, 0) != usize::MAX / 0xff * 0xdd || get(base + WORD, 0) != 0x33 {
        return Err("a quarantined block wasn't filled, or wasn't marked");
    }
    Ok(())
}

// ---- Six mistakes - ----------------------------------------------------------------------

/// What came of a mistake: the fault the allocator returned, if it returned one, and a
/// line saying where, or what happened instead.
type Outcome = (Option<Fault>, String);

fn caught(by: &str, fault: Fault) -> Outcome {
    (Some(fault), format!("caught by {by}: {fault:?}"))
}

fn heap<const D: u8>(base: usize) -> Hardened<D> {
    // SAFETY: the region is the test's, and one allocator uses it at a time.
    unsafe { Hardened::new(base, REGION, SEED) }
}

fn take<const D: u8>(heap: &mut Hardened<D>, size: usize) -> usize {
    heap.allocate(size, 8).expect("a new heap is corrupt").expect("a new heap is full")
}

fn ordinal(n: usize) -> String {
    format!("{n}{}", ["th", "st", "nd", "rd"][if n % 100 / 10 == 1 || n % 10 > 3 { 0 } else { n % 10 }])
}

/// A block freed twice in a row.
fn freed_twice<const D: u8>(base: usize) -> Outcome {
    let mut heap = heap::<D>(base);
    let [_a, b, _c] = [0; 3].map(|_| take(&mut heap, 48));
    match unsafe { (heap.free(b), heap.free(b)) } {
        (Ok(()), Err(fault)) => caught("the second free", fault),
        _ => (None, String::from("not caught")),
    }
}

/// A write past the end of a block of 24 bytes, four words long, which puts back the
/// value of each tag it crosses: `a`'s footer and `b`'s header. Its last word is `b`'s
/// first.
fn overrun<const D: u8>(base: usize) -> Outcome {
    let mut heap = heap::<D>(base);
    let [a, b] = [0; 2].map(|_| take(&mut heap, 24));
    put(b, 0, 2000);
    let tag = get(a, 4);
    for (word, value) in [(3, tag), (4, tag), (5, tag), (6, 0x6666)] {
        put(a, word, value);
    }
    match unsafe { heap.free(a) } {
        Err(fault) => caught("the free of the block", fault),
        Ok(()) => (None, format!("not caught: the next block's first word went from 2000 to {:#x}", get(b, 0))),
    }
}

/// What a write to a freed block leaves in its first word, where the allocator keeps the
/// link to the next free block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Forgery {
    /// The block's own header: a free block inside the region, as a link should be.
    Itself,
    /// Something outside the region made to look like a free block.
    Outside,
    /// The same inside a block the writer holds, with a back link to the freed block.
    Inside,
}

/// The logger's mistake. `f` is freed and its first word is then written. Then two
/// allocations of `f`'s size, and then the program goes on freeing other blocks.
fn stale_link<const D: u8>(base: usize, forgery: Forgery) -> Outcome {
    static OUTSIDE: Global<[usize; 8]> = Global::new([0; 8]);
    let mut heap = heap::<D>(base);
    let own = take(&mut heap, 96);
    let [f, _g] = [0; 2].map(|_| take(&mut heap, 48));
    let others: Vec<usize> = (0..64).map(|_| take(&mut heap, 48)).collect();
    unsafe { heap.free(f).expect("an allocated block couldn't be freed") };
    let fake = match forgery {
        Forgery::Itself => f - WORD,
        Forgery::Outside => &raw const OUTSIDE as usize,
        Forgery::Inside => own + WORD,
    };
    if forgery != Forgery::Itself {
        forge(fake, f - WORD);
    }
    put(f, 0, fake);
    let mut got = [0; 2];
    for addr in &mut got {
        match heap.allocate(48, 8) {
            Err(fault) => return caught("the next allocation", fault),
            Ok(block) => *addr = block.unwrap_or(0),
        }
    }
    if got[1] == fake + WORD {
        let what = match forgery {
            Forgery::Itself => "the block was handed out twice",
            Forgery::Outside => "the second allocation is outside the region",
            Forgery::Inside => "the second allocation is inside another block",
        };
        return (None, format!("not caught: {what}"));
    }
    for (i, &other) in others.iter().enumerate() {
        if let Err(fault) = unsafe { heap.free(other) } {
            return caught(&format!("the {} free after it", ordinal(i + 1)), fault);
        }
    }
    match heap.drain() {
        Err(fault) => caught("emptying the quarantine", fault),
        Ok(()) => (None, String::from("not caught, and the forged link wasn't followed")),
    }
}

/// A block freed, handed out again, and then written through the address its first owner
/// kept. Nothing here is the allocator's to see: the block is allocated, and the write is
/// inside it.
fn after_reuse<const D: u8>(base: usize) -> Outcome {
    let mut heap = heap::<D>(base);
    let stale = take(&mut heap, 48);
    let _guard = take(&mut heap, 48);
    unsafe { heap.free(stale).expect("an allocated block couldn't be freed") };
    let mut held: Vec<usize> = Vec::new();
    let mut allocations = 0;
    let reused = loop {
        let block = take(&mut heap, 48);
        allocations += 1;
        if block == stale {
            break block;
        }
        if allocations == 4096 {
            return (None, String::from("the block wasn't handed out again in 4096 allocations"));
        }
        held.push(block);
        if held.len() > 4 {
            unsafe { heap.free(held.remove(0)).expect("an allocated block couldn't be freed") };
        }
    };
    put(reused, 0, 2000);
    put(stale, 0, 0x6666);
    let end = unsafe { heap.free(reused) }.and_then(|()| heap.drain());
    match end {
        Err(fault) => caught("a later free", fault),
        Ok(()) => (None, format!("not caught: handed out again by allocation {allocations}, then written")),
    }
}

/// A mistake, what came of it under each of the six allocators, and the fault each should
/// have returned.
type Row = (&'static str, [Outcome; 6], [Option<Fault>; 6]);

fn mistakes(base: usize) -> Result<(), &'static str> {
    use Fault::{Canary, Link, NotAllocated, Unlink, WrittenAfterFree as Written};
    use Forgery::{Inside, Itself, Outside};
    let rows: [Row; 6] = [
        ("a block freed twice:", each!(freed_twice(base)), [Some(NotAllocated); 6]),
        (
            "a write past the end of a block that leaves the tags as they were:",
            each!(overrun(base)),
            [None, None, None, Some(Canary), Some(Canary), Some(Canary)],
        ),
        (
            "a write to a freed block: a link to the block itself:",
            each!(stale_link(base, Itself)),
            [None, Some(Unlink), Some(Link), Some(Link), Some(Written), Some(Written)],
        ),
        (
            "a write to a freed block: a link to a forged block outside the region:",
            each!(stale_link(base, Outside)),
            [None, Some(Link), Some(Link), Some(Link), Some(Written), Some(Written)],
        ),
        (
            "a write to a freed block: a link to a forged block inside the region, which points back:",
            each!(stale_link(base, Inside)),
            [None, None, Some(Link), Some(Link), Some(Written), Some(Written)],
        ),
        ("a write to a freed block after it has been handed out again:", each!(after_reuse(base)), [None; 6]),
    ];
    println!("six mistakes, against the allocator with one defence more each time:");
    let mut as_expected = true;
    for (mistake, outcomes, expected) in rows {
        println!("{mistake}");
        for ((name, (fault, what)), expected) in NAMES.iter().zip(outcomes).zip(expected) {
            println!("  {name:17} {what}");
            as_expected &= fault == expected;
        }
    }
    if !as_expected {
        return Err("a defence didn't catch what it should, or caught what it shouldn't be able to see");
    }
    Ok(())
}

// ---- When and where a freed block is used again -----------------------------------------

/// Free a block, then allocate and free others of its size, and count the allocations
/// until one is given the freed block's memory. 200 times; the counts, sorted.
fn delays<const D: u8>(base: usize) -> Vec<usize> {
    let mut heap = heap::<D>(base);
    let mut random = Random(SEED);
    let mut live: Vec<usize> = (0..256).map(|_| take(&mut heap, 48)).collect();
    let mut delays = Vec::with_capacity(200);
    for _ in 0..200 {
        let freed = live.swap_remove(random.below(live.len()));
        unsafe { heap.free(freed).expect("an allocated block couldn't be freed") };
        let mut count = 0;
        loop {
            count += 1;
            let block = take(&mut heap, 48);
            live.push(block);
            if block.abs_diff(freed) < 64 || count == 100_000 {
                break;
            }
            let other = live.swap_remove(random.below(live.len()));
            unsafe { heap.free(other).expect("an allocated block couldn't be freed") };
        }
        delays.push(count);
    }
    delays.sort_unstable();
    delays
}

/// Free every other one of 512 blocks, allocate 64 of that size, and count how many of
/// them went to the hole beside the hole the one before it went to.
fn neighbours<const D: u8>(base: usize) -> usize {
    let mut heap = heap::<D>(base);
    let blocks: Vec<usize> = (0..512).map(|_| take(&mut heap, 48)).collect();
    for &block in blocks.iter().step_by(2) {
        unsafe { heap.free(block).expect("an allocated block couldn't be freed") };
    }
    heap.drain().expect("a quarantined block was written to");
    let placed: Vec<usize> = (0..64).map(|_| take(&mut heap, 48)).collect();
    placed.windows(2).filter(|pair| pair[0].abs_diff(pair[1]) <= 128).count()
}

/// Returns the delay with a quarantine and no randomness, and the least and the most
/// with randomness.
fn reuse(base: usize) -> Result<(usize, usize, usize), &'static str> {
    println!("a block of 48 bytes is freed; allocations until one is given its memory, in 200 trials:");
    println!("  allocator          least  median   most  different counts");
    let all = [delays::<NONE>(base), delays::<L4>(base), delays::<L5>(base)];
    for (name, delays) in ["no defence", "+ quarantine", "+ random"].iter().zip(&all) {
        let mut different = delays.clone();
        different.dedup();
        println!("  {name:17} {:6} {:7} {:6} {:17}", delays[0], delays[100], delays[199], different.len());
    }
    let (plain, fifo, random) = (&all[0], &all[1], &all[2]);
    if (plain[0], plain[199]) != (1, 1) || (fifo[0], fifo[199]) != (17, 17) {
        return Err("without randomness, a freed block should be reused after a fixed number of allocations");
    }
    if random[0] == random[199] || random[199] == 100_000 {
        return Err("with randomness, the delay before a block is reused should vary, and end");
    }
    let (lifo, spread) = (neighbours::<NONE>(base), neighbours::<L5>(base));
    println!("256 holes of 64 bytes; of 63 allocations, the number placed beside the one before:");
    println!("  no defence        {lifo}");
    println!("  + random          {spread}");
    if lifo != 63 || spread > 8 {
        return Err("a random choice should scatter allocations that a LIFO list places side by side");
    }
    Ok((fifo[0], random[0], random[199]))
}

// ---- What the defences cost ---------------------------------------------------------------

/// A count divided by another, to one decimal place, without floating point.
fn ratio(count: usize, per: usize) -> String {
    let tenths = (count * 10 + per / 2) / per.max(1);
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// The numbers of one replay: the peak of bytes in blocks, the peak in quarantine, the
/// footprint, free blocks looked at, and words poisoned.
struct Cost {
    peak_used: usize,
    peak_held: usize,
    footprint: usize,
}

fn replayed<const D: u8>(base: usize, events: &[trace::Event], slots: usize, name: &str) -> Result<Cost, &'static str> {
    let mut heap = heap::<D>(base);
    heap.set_quarantine(slots);
    let report = trace::replay(events, &mut heap);
    heap.check()?;
    let (stats, counts) = (heap.stats(), heap.counts());
    println!(
        "  {name:17} {:10} {:10} {:10} {:>10} {:>12}",
        report.peak_used,
        counts.peak_held,
        report.footprint,
        ratio(stats.examined, stats.allocations),
        ratio(counts.poisoned + counts.verified, stats.frees),
    );
    if report.failed != 0 {
        return Err("the workload didn't fit in its region");
    }
    Ok(Cost { peak_used: report.peak_used, peak_held: counts.peak_held, footprint: report.footprint })
}

/// Returns the footprint of the workload with no defence and with all five.
fn costs(base: usize) -> Result<(usize, usize), &'static str> {
    println!("the block for a request, in bytes:");
    println!("  request       0    8    9   16   24   25   32   48   64  100");
    for (name, size) in [
        ("no canary", Hardened::<NONE>::block_size as fn(usize) -> usize),
        ("a canary", Hardened::<L3>::block_size as fn(usize) -> usize),
    ] {
        let sizes = [0, 8, 9, 16, 24, 25, 32, 48, 64, 100].map(|request| format!("{:5}", size(request)));
        println!("  {name:10}{}", sizes.concat());
    }

    let events = random_trace();
    println!("the fit test's 20000 allocations and frees, replayed in a region of {} KiB:", REGION / 1024);
    println!("  allocator          peak used  peak held  footprint  looked at  words filled");
    println!("                                                     per alloc  and checked");
    println!("                                                                per free");
    let rows = [
        replayed::<NONE>(base, &events, 16, NAMES[0])?,
        replayed::<L1>(base, &events, 16, NAMES[1])?,
        replayed::<L2>(base, &events, 16, NAMES[2])?,
        replayed::<L3>(base, &events, 16, NAMES[3])?,
        replayed::<L4>(base, &events, 16, NAMES[4])?,
        replayed::<L5>(base, &events, 16, NAMES[5])?,
    ];
    if rows[1].footprint != rows[0].footprint || rows[2].footprint != rows[0].footprint {
        return Err("checking and encoding links shouldn't change where blocks go");
    }
    if rows[3].peak_used <= rows[2].peak_used || rows[4].peak_held == 0 || rows[3].peak_held != 0 {
        return Err("a canary should cost bytes in blocks, and only a quarantine should hold any");
    }
    println!("with a quarantine of 0, 4, 16 and 64 blocks, and no random choice:");
    let mut last = 0;
    for slots in [0, 4, 16, 64] {
        let cost = replayed::<L4>(base, &events, slots, &format!("{slots} blocks"))?;
        if cost.peak_held < last {
            return Err("a longer quarantine should hold more");
        }
        last = cost.peak_held;
    }
    Ok((rows[0].footprint, rows[5].footprint))
}
