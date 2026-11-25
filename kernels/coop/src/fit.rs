//! `cargo xtask kernel --kernel fit`: what green-tea-fit's allocator does with a region,
//! printed, and then checked.
//!
//! - The words of a small region after two allocations and a free: the tags and the links.
//! - Blocks freed one by one, merging with their neighbours.
//! - One request given to first fit, next fit and best fit, over the same free blocks.
//! - The caller's mistakes: which the allocator catches, and what the others do.
//! - The same 20,000 allocations and frees under each policy and list order, counted.
//! - The kernel's own heap, which is one of these: a thousand tasks leave it as it was.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use green_tea_bare::fit::{Fit, FreeError, MIN_BLOCK, Order, Policy, Stats};
use green_tea_bare::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::{Global, heap, println};
use green_tea_scheduler_coop::runtime::{run, spawn};
use green_tea_scheduler_coop::task::yield_now;

const WORD: usize = size_of::<usize>();
const TASKS: usize = 1000;

pub fn test(fb: Framebuffer) {
    let verdict = words()
        .and_then(|()| merging())
        .and_then(|()| policies())
        .and_then(|()| mistakes())
        .and_then(|()| workloads())
        .and_then(|()| kernel_heap());
    match verdict {
        Err(e) => println!("FAIL: {e}"),
        Ok(()) => {
            Screen::new(fb).finish(TASKS);
            println!(
                "PASS: freed blocks merged and were reused; {TASKS} tasks and six workloads left their heaps as they \
                 found them"
            );
        }
    }
}

/// `len` bytes for an allocator to manage, from the kernel's heap, and their address.
fn region(len: usize) -> (Vec<u128>, usize) {
    let mut memory = vec![0u128; len / 16];
    let base = memory.as_mut_ptr() as usize;
    (memory, base)
}

/// The region's blocks in address order, by size, the free ones in parentheses.
fn map(fit: &Fit) -> String {
    let mut map = String::new();
    for block in fit.blocks() {
        let _ = if block.used { write!(map, "{} ", block.size) } else { write!(map, "({}) ", block.size) };
    }
    map.pop();
    map
}

fn dump(from: usize, words: usize) {
    for addr in (from..).step_by(WORD).take(words) {
        println!("  {addr:#x}  {:#018x}", unsafe { (addr as *const usize).read() });
    }
}

fn fill(addr: usize, len: usize, byte: u8) {
    unsafe { core::ptr::write_bytes(addr as *mut u8, byte, len) };
}

fn words() -> Result<(), &'static str> {
    let (_memory, base) = region(176);
    let mut fit = unsafe { Fit::new(base, 176, Policy::FirstFit, Order::Address) };
    let [a, b] = [0x61, 0x62].map(|byte| {
        let addr = fit.allocate(24, 8).unwrap();
        fill(addr, 24, byte);
        addr
    });
    println!("a region of {} bytes, after a = allocate(24) and b = allocate(24):", fit.capacity());
    println!("  a = {a:#x}, b = {b:#x}; the region, a word to a line:");
    dump(fit.first(), fit.capacity() / WORD);
    unsafe { fit.free(a).map_err(|_| "a block just allocated couldn't be freed")? };
    println!("after free(a), a's block:");
    dump(fit.first(), 48 / WORD);
    if a != fit.first() + WORD || b != a + 48 || map(&fit) != "(48) 48 (64)" {
        return Err("two 24-byte blocks weren't two 48-byte blocks at the start of the region");
    }
    fit.check()
}

fn merging() -> Result<(), &'static str> {
    let (_memory, base) = region(512);
    let mut fit = unsafe { Fit::new(base, 512, Policy::FirstFit, Order::Address) };
    println!("a region of {} bytes; free blocks are in parentheses:", fit.capacity());
    println!("  new:                 {}", map(&fit));
    let [a, b, c, d, e] = [0; 5].map(|_| fit.allocate(48, 8).unwrap());
    println!("  five blocks, a to e: {}", map(&fit));
    let mut maps = Vec::new();
    for (name, blocks) in [("b and d", &[b, d][..]), ("c", &[c]), ("e", &[e]), ("a", &[a])] {
        for &block in blocks {
            unsafe { fit.free(block).map_err(|_| "an allocated block couldn't be freed")? };
        }
        println!("  {:21}{}", alloc::format!("{name} freed:"), map(&fit));
        fit.check()?;
        maps.push(map(&fit));
    }
    println!("  free blocks merged: {}", fit.stats().merges);
    if maps != ["64 (64) 64 (64) 64 (176)", "64 (192) 64 (176)", "64 (432)", "(496)"] || fit.stats().merges != 5 {
        return Err("freed blocks didn't merge with their free neighbours");
    }
    Ok(())
}

/// Free blocks of 64, 256 and 96 bytes in address order, kept apart by allocated blocks of
/// 32, and the rest of a 4 KiB region free above them.
fn three_holes(policy: Policy, order: Order, memory: &mut Vec<u128>) -> (Fit, [usize; 3]) {
    let mut fit = unsafe { Fit::new(memory.as_mut_ptr() as usize, 4096, policy, order) };
    let blocks = [64 - 16, 16, 256 - 16, 16, 96 - 16, 16].map(|size| fit.allocate(size, 8).unwrap());
    for hole in [0, 2, 4] {
        unsafe { fit.free(blocks[hole]).unwrap() };
    }
    (fit, [blocks[0], blocks[2], blocks[4]])
}

fn policies() -> Result<(), &'static str> {
    let (mut memory, _) = region(4096);
    let (fit, _) = three_holes(Policy::FirstFit, Order::Address, &mut memory);
    println!("three holes and the rest of a region: {}", map(&fit));
    println!("a request for 64 bytes, which takes a block of 80:");
    let mut chosen = [0; 3];
    for (i, (policy, name)) in
        [(Policy::FirstFit, "first fit"), (Policy::NextFit, "next fit"), (Policy::BestFit, "best fit")]
            .into_iter()
            .enumerate()
    {
        let (mut fit, holes) = three_holes(policy, Order::Address, &mut memory);
        let before = fit.stats().examined;
        let got = fit.allocate(64, 8).ok_or("a request that fits was refused")?;
        chosen[i] = holes.iter().position(|&hole| hole == got).map_or(0, |hole| [64, 256, 96][hole]);
        println!("  {name:10} looked at {} free blocks: {}", fit.stats().examined - before, map(&fit));
        fit.check()?;
    }
    if chosen != [256, 0, 96] {
        return Err("the three policies didn't choose the blocks they should");
    }

    println!("three blocks of 64 freed, the middle one last; the next request for 48 bytes gets:");
    let mut got = [0; 2];
    for (i, (order, name)) in [(Order::Lifo, "LIFO order"), (Order::Address, "address order")].into_iter().enumerate() {
        let mut fit = unsafe { Fit::new(memory.as_mut_ptr() as usize, 4096, Policy::FirstFit, order) };
        let blocks = [0; 6].map(|_| fit.allocate(48, 8).unwrap());
        for freed in [0, 4, 2] {
            unsafe { fit.free(blocks[freed]).unwrap() };
        }
        let block = fit.allocate(48, 8).unwrap();
        got[i] = blocks.iter().position(|&b| b == block).unwrap_or(usize::MAX);
        println!("  {name:14} block {}, the {}", got[i], if got[i] == 2 { "last one freed" } else { "lowest" });
    }
    if got != [2, 0] {
        return Err("the two list orders didn't reuse the blocks they should");
    }
    Ok(())
}

fn mistakes() -> Result<(), &'static str> {
    let (_memory, base) = region(4096);
    let mut fit = unsafe { Fit::new(base, 4096, Policy::FirstFit, Order::Lifo) };
    let [a, b, c, d, e, f, _g] = [0; 7].map(|_| fit.allocate(48, 8).unwrap());
    println!("the caller's mistakes:");

    let twice = unsafe { (fit.free(b), fit.free(b)) };
    println!("  b freed twice:                          {:?}, then {:?}", twice.0, twice.1);
    // a merges with b. Then d is freed, and c between them: c merges on both sides, and
    // neither of its tags is a tag of the merged block.
    let merged = unsafe { (fit.free(a), fit.free(d), fit.free(c), fit.free(c)) };
    println!("  c freed between free blocks, and again: {:?}, then {:?}", merged.2, merged.3);
    let inside = unsafe { fit.free(e + 8) };
    println!("  an address inside e freed:              {inside:?}");
    // One word too many: the 49th to 56th bytes of a 48-byte block are its footer.
    unsafe { ((e + 48) as *mut usize).write(0x4141_4141_4141_4141) };
    let overrun = unsafe { fit.free(e) };
    println!("  e freed after a write past its end:     {overrun:?}");
    println!("  and the check of the whole region:      {:?}", fit.check());
    if twice != (Ok(()), Err(FreeError::NotAllocated))
        || merged != (Ok(()), Ok(()), Ok(()), Err(FreeError::NotAllocated))
        || inside != Err(FreeError::NotABlock)
        || overrun != Err(FreeError::Corrupt)
        || fit.check().is_ok()
    {
        return Err("the allocator didn't refuse a mistake it can see");
    }

    // A mistake it can't see: a write to a block after it was freed. `f` is freed, and
    // then something that kept its address writes there the address of a variable of
    // its own, made to look like a free block of 64 bytes.
    let victim = [64usize, 0, 0, 0, 0, 0, 0, 64];
    let fake = core::hint::black_box(&raw const victim) as usize;
    unsafe {
        fit.free(f).map_err(|_| "an allocated block couldn't be freed")?;
        (f as *mut usize).write(fake); // where the allocator keeps f's link to the next free block
    }
    let got = [0; 2].map(|_| fit.allocate(48, 8));
    let outside = got[1] == Some(fake + WORD);
    println!("  f written to after it was freed; the next two blocks of 48 bytes:");
    println!("    the first is f: {}; the second is outside the region: {outside}", got[0] == Some(f));
    if got[0] != Some(f) || !outside {
        return Err("a write to a freed block should have redirected the free list");
    }
    Ok(())
}

/// What the workload asks of an allocator. Changing any of these changes every number
/// the test prints.
const REGION: usize = 256 * 1024;
const OPERATIONS: usize = 20_000;
const LIVE: usize = 400;

/// A linear congruential generator: the same numbers on every board, every run.
struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as usize % n
    }
}

struct Outcome {
    stats: Stats,
    /// The free list's length and the largest free block, when the last operation is done.
    free_blocks: usize,
    largest_free: usize,
}

/// `OPERATIONS` allocations and frees. Seven requests in eight are for 16 to 127 bytes,
/// the rest for 128 to 2047. While fewer than `LIVE` blocks are allocated, two operations
/// in three allocate; from then on allocations and frees alternate at random. The block
/// freed is any of the live ones.
fn workload(policy: Policy, order: Order, memory: &mut Vec<u128>) -> Result<Outcome, &'static str> {
    let mut fit = unsafe { Fit::new(memory.as_mut_ptr() as usize, REGION, policy, order) };
    let mut random = Random(0x2545_f491_4f6c_dd1d);
    let mut live: Vec<(usize, usize)> = Vec::with_capacity(LIVE + 1);
    for step in 0..OPERATIONS {
        let allocate = live.is_empty() || random.below(if live.len() < LIVE { 3 } else { 2 }) != 0;
        if allocate && live.len() <= LIVE {
            let size = if random.below(8) == 0 { 128 + random.below(1920) } else { 16 + random.below(112) };
            if let Some(addr) = fit.allocate(size, 8) {
                fill(addr, size, step as u8);
                live.push((addr, size));
            }
        } else {
            let (addr, size) = live.swap_remove(random.below(live.len()));
            let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
            if bytes.iter().any(|&byte| byte != bytes[0]) {
                return Err("two blocks overlapped");
            }
            unsafe { fit.free(addr).map_err(|_| "an allocated block couldn't be freed")? };
        }
    }
    fit.check()?;
    let outcome =
        Outcome { stats: fit.stats(), free_blocks: fit.free_list().count(), largest_free: fit.largest_free() };
    for (addr, _) in live {
        unsafe { fit.free(addr).map_err(|_| "an allocated block couldn't be freed")? };
    }
    if fit.largest_free() != fit.capacity() {
        return Err("a region with nothing allocated wasn't one free block");
    }
    Ok(outcome)
}

/// A count divided by another, to one decimal place, without floating point.
fn ratio(count: usize, per: usize) -> String {
    let tenths = (count * 10 + per / 2) / per.max(1);
    alloc::format!("{}.{}", tenths / 10, tenths % 10)
}

fn workloads() -> Result<(), &'static str> {
    let (mut memory, _) = region(REGION);
    println!("{OPERATIONS} allocations and frees in a region of {} KiB, up to {LIVE} blocks at once:", REGION / 1024);
    println!(
        "  policy     order    allocs  frees  looked at  walked past  splits  merges  peak used  free blocks  largest"
    );
    println!(
        "                                     per alloc     per free                      bytes       at end   at end"
    );
    for (policy, policy_name) in
        [(Policy::FirstFit, "first fit"), (Policy::NextFit, "next fit"), (Policy::BestFit, "best fit")]
    {
        for (order, order_name) in [(Order::Lifo, "LIFO"), (Order::Address, "address")] {
            let Outcome { stats: s, free_blocks, largest_free } = workload(policy, order, &mut memory)?;
            println!(
                "  {policy_name:10} {order_name:7} {:7} {:6} {:>10} {:>12} {:7} {:7} {:10} {free_blocks:12} {largest_free:8}",
                s.allocations,
                s.frees,
                ratio(s.examined, s.allocations),
                ratio(s.passed, s.frees),
                s.splits,
                s.merges,
                s.peak,
            );
            if s.failed != 0 || s.allocations + s.frees != OPERATIONS {
                return Err("the workload didn't fit in its region");
            }
            if s.examined < s.allocations {
                return Err("an allocation was made without looking at a free block");
            }
        }
    }
    Ok(())
}

fn kernel_heap() -> Result<(), &'static str> {
    static DURING: Global<(usize, usize, usize)> = Global::new((0, 0, 0));
    let (used, blocks, free_blocks) = heap_now();
    run(
        || {
            for task in 0..TASKS {
                spawn(move || {
                    if task == TASKS / 2 {
                        DURING.set(heap_now());
                    }
                });
                yield_now(); // the new task runs, and finishes, before the next is spawned
            }
        },
        |_| {},
    );
    let (after, during) = (heap_now(), DURING.get());
    let stats = heap::with(|fit| fit.stats());
    println!(
        "the kernel's heap ({}): {used} of {} bytes in {blocks} blocks, {free_blocks} free blocks",
        heap::NAME,
        heap::capacity()
    );
    println!(
        "while task {} of {TASKS} ran: {} bytes in {} blocks, {} free blocks",
        TASKS / 2,
        during.0,
        during.1,
        during.2
    );
    println!("after all {TASKS}: {} bytes in {} blocks, {} free blocks", after.0, after.1, after.2);
    println!(
        "since boot: {} allocations, {} frees, {} free blocks looked at per allocation, the most in use {} bytes",
        stats.allocations,
        stats.frees,
        ratio(stats.examined, stats.allocations),
        stats.peak
    );
    heap::with(|fit| fit.check())?;
    if during.1 == 0 {
        return Err("a running task had nothing on the kernel's heap");
    }
    if after != (used, blocks, free_blocks) {
        return Err("a thousand tasks left something on the kernel's heap");
    }
    if MIN_BLOCK != 32 {
        return Err("the smallest block isn't 32 bytes");
    }
    Ok(())
}

/// Bytes in use, allocated blocks and free blocks on the kernel's heap.
fn heap_now() -> (usize, usize, usize) {
    heap::with(|fit| (fit.used(), fit.blocks().filter(|b| b.used).count(), fit.free_list().count()))
}
