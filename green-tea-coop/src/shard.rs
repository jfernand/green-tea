//! The test of allocation from several CPUs without a lock, `cargo xtask kernel --kernel shard`.
//!
//! The kernel starts the board's other CPUs, and all four do the same work twice: through a
//! cache each in front of a heap behind a lock (green-tea-smp, as the `smp` test does), and
//! through a heap of pages each (green-tea-shard), where a free list belongs to a page and
//! nothing is locked.
//!
//! The work is of two kinds. In the first, a CPU frees only what it allocated. In the
//! second, every block a CPU allocates is freed by one of the other three.
//!
//! Only the boot CPU prints. The others leave what they counted in atomics, or in the
//! heap that the boot CPU made for them.

use alloc::boxed::Box;
use alloc::vec;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use green_tea_bare::fit::tlsf::Tlsf;
use green_tea_bare::fit::trace::Random;
use green_tea_bare::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::smp::cache::{Cache, Central};
use green_tea_bare::smp::lock::SpinLock;
use green_tea_bare::{Board, Platform, cpus, println};
use green_tea_shard::{Arena, Heap, PAGE, Stats};

/// The CPUs `cargo xtask kernel` gives the machine for this test (`-smp 4`).
const CPUS: usize = 4;

/// The first kind of work, which is the `smp` test's: how many allocations and frees each
/// CPU makes, the most blocks it holds at once, and the largest block it asks for.
const OPERATIONS: usize = 20_000;
const HELD: usize = 96;
const LARGEST: usize = 512;

/// The second kind: how many blocks a CPU allocates in a round, and how many rounds.
const PASSED: usize = 48;
const ROUNDS: usize = 200;

/// The caches move 8 blocks at a time and keep at most 16 of a class.
const BATCH: usize = 8;
const LIMIT: usize = 16;

/// The region the caches' heap is given, from the kernel's heap, and the arena's.
const REGION: usize = 1024 * 1024;
const ARENA: usize = 2 * 1024 * 1024;

/// The heap the caches are in front of, and each CPU's cache.
static CENTRAL: AtomicPtr<SpinLock<Central<Tlsf>>> = AtomicPtr::new(core::ptr::null_mut());
static CACHES: [AtomicPtr<Cache>; CPUS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; CPUS];
/// Each CPU's heap of pages.
static HEAPS: [AtomicPtr<Heap<'static>>; CPUS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; CPUS];

/// What the CPUs counted, added up.
static DONE: AtomicUsize = AtomicUsize::new(0); // allocations and frees made
static WRONG: AtomicUsize = AtomicUsize::new(0); // blocks whose bytes had changed
static REFUSED: AtomicUsize = AtomicUsize::new(0); // allocations and frees refused

/// The blocks each CPU left for the others in a round, and their sizes. Rounds alternate
/// between the two halves, so that a CPU can read last round's while others write this one's.
static LEFT: [[[AtomicUsize; PASSED]; CPUS]; 2] =
    [const { [const { [const { AtomicUsize::new(0) }; PASSED] }; CPUS] }; 2];
static SIZES: [[[AtomicUsize; PASSED]; CPUS]; 2] =
    [const { [const { [const { AtomicUsize::new(0) }; PASSED] }; CPUS] }; 2];
static ROUND: AtomicUsize = AtomicUsize::new(0);

pub fn test(fb: Framebuffer) {
    match run() {
        Err(e) => println!("FAIL: {e}"),
        Ok((locks, remote)) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: {CPUS} CPUs allocated through a heap of pages each and took no lock, where their caches took one \
                 {locks} times; {remote} blocks were freed by a CPU that didn't allocate them, and every one went back \
                 to its page"
            );
        }
    }
    cpus::park();
}

fn run() -> Result<(usize, usize), &'static str> {
    let running = cpus::start_all();
    println!("{running} CPUs are running: the one that booted, and {} it started", running - 1);
    if running != CPUS {
        return Err("the machine should have 4 CPUs: run it with -smp 4");
    }
    println!("each CPU makes {OPERATIONS} allocations and frees, of 1 to {LARGEST} bytes, holding up to {HELD}:");
    let churn_locks = with_caches(Work::Churn)?;
    with_heaps(Work::Churn, 1)?;
    with_heaps(Work::Churn, 0)?;
    println!("each CPU allocates {PASSED} blocks and leaves them for the other three to free, {ROUNDS} times:");
    let pass_locks = with_caches(Work::Pass)?;
    let remote = with_heaps(Work::Pass, 1)?;
    Ok((churn_locks + pass_locks, remote))
}

#[derive(Clone, Copy, PartialEq)]
enum Work {
    /// Each CPU frees what it allocated.
    Churn,
    /// Each CPU frees what the others allocated.
    Pass,
}

fn ms(ticks: u64) -> u64 {
    ticks / Board::ticks_per_ms()
}

/// Use this CPU's cache and the heap behind it without letting either reference escape.
fn with_cache<R>(cpu: usize, f: impl FnOnce(&mut Cache, &SpinLock<Central<Tlsf>>) -> R) -> R {
    let cache = CACHES[cpu].load(Ordering::Acquire);
    let central = CENTRAL.load(Ordering::Acquire);
    assert!(!cache.is_null() && !central.is_null(), "the caches haven't been initialized");
    // SAFETY: `with_caches` initializes these before work and clears them after; one CPU
    // exclusively uses its cache, and `central` is synchronized by its lock.
    unsafe { f(&mut *cache, &*central) }
}

/// Use this CPU's heap of pages without letting its reference escape.
fn with_heap<R>(cpu: usize, f: impl FnOnce(&mut Heap<'static>) -> R) -> R {
    let heap = HEAPS[cpu].load(Ordering::Acquire);
    assert!(!heap.is_null(), "the heaps haven't been initialized");
    // SAFETY: `with_heaps` initializes the heap before work and clears it after; each heap
    // is accessed only by its CPU, except after the workers have joined.
    unsafe { f(&mut *heap) }
}

// ---- The first kind of work: a CPU frees what it allocated ----------------------------

/// `OPERATIONS` allocations and frees, in ramps: allocate blocks of 1 to `LARGEST` bytes
/// until `HELD` are held, free them in a random order until none is, and again. Every byte
/// of a block is set to the CPU's number, and checked before the block is freed. The same
/// requests, from the same seeds, as the `smp` test makes.
fn churn(cpu: usize, mut allocate: impl FnMut(usize) -> Option<usize>, mut free: impl FnMut(usize, usize)) {
    let mut random = Random(cpu as u64 + 1);
    let mut held = [(0usize, 0usize); HELD];
    let mut holding = 0;
    let mut done = 0;
    let mut rising = true;
    for _ in 0..OPERATIONS {
        if holding == 0 || holding == HELD {
            rising = holding == 0;
        }
        if rising {
            let size = 1 + random.below(LARGEST);
            let Some(addr) = allocate(size) else {
                REFUSED.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            fill(addr, size, cpu);
            held[holding] = (addr, size);
            holding += 1;
        } else {
            let which = random.below(holding);
            holding -= 1;
            inspect(held[which].0, held[which].1, cpu);
            free(held[which].0, held[which].1);
            held[which] = held[holding];
        }
        done += 1;
    }
    for &(addr, size) in &held[..holding] {
        inspect(addr, size, cpu);
        free(addr, size);
        done += 1;
    }
    DONE.fetch_add(done, Ordering::Relaxed);
}

fn fill(addr: usize, size: usize, owner: usize) {
    // SAFETY: the allocator gave us `size` bytes there.
    unsafe { core::ptr::write_bytes(addr as *mut u8, owner as u8 + 1, size) };
}

/// Count the block as wrong unless every byte is still its owner's number.
fn inspect(addr: usize, size: usize, owner: usize) {
    // SAFETY: the block is allocated, and nobody writes it until it is freed.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    if bytes.iter().any(|&byte| byte != owner as u8 + 1) {
        WRONG.fetch_add(1, Ordering::Relaxed);
    }
}

// ---- The second kind: a CPU frees what the others allocated ---------------------------

/// One round. Free the blocks that the other CPUs left for this one in the round before,
/// then allocate `PASSED` blocks and leave them: block `j` for CPU `cpu + 1 + j % 3`. In
/// the round after the last, only free.
fn pass(cpu: usize, mut allocate: impl FnMut(usize) -> Option<usize>, mut free: impl FnMut(usize, usize)) {
    let round = ROUND.load(Ordering::Acquire);
    let (now, before) = (round % 2, (round + 1) % 2);
    let mut done = 0;
    if round > 0 {
        for from in (0..CPUS).filter(|&from| from != cpu) {
            for j in (0..PASSED).filter(|j| (from + 1 + j % 3) % CPUS == cpu) {
                let addr = LEFT[before][from][j].load(Ordering::Acquire);
                let size = SIZES[before][from][j].load(Ordering::Acquire);
                inspect(addr, size, from);
                free(addr, size);
                done += 1;
            }
        }
    }
    if round < ROUNDS {
        let mut random = Random((round * CPUS + cpu) as u64 + 1);
        for j in 0..PASSED {
            let size = 1 + random.below(LARGEST);
            let Some(addr) = allocate(size) else {
                REFUSED.fetch_add(1, Ordering::Relaxed);
                return;
            };
            fill(addr, size, cpu);
            SIZES[now][cpu][j].store(size, Ordering::Release);
            LEFT[now][cpu][j].store(addr, Ordering::Release);
            done += 1;
        }
    }
    DONE.fetch_add(done, Ordering::Relaxed);
}

/// Clear the counts, give every CPU `work` (once, or once for each round and once more),
/// and return how many operations were made and how long it took.
fn measure(kind: Work, work: fn(usize)) -> Result<(usize, u64), &'static str> {
    for count in [&DONE, &WRONG, &REFUSED] {
        count.store(0, Ordering::Relaxed);
    }
    let start = Board::ticks();
    match kind {
        Work::Churn => cpus::run(work),
        Work::Pass => {
            for round in 0..=ROUNDS {
                ROUND.store(round, Ordering::Release);
                cpus::run(work);
            }
        }
    }
    let took = ms(Board::ticks() - start);
    if WRONG.load(Ordering::Relaxed) != 0 {
        return Err("a block's bytes changed while it was allocated: it was handed out twice");
    }
    if REFUSED.load(Ordering::Relaxed) != 0 {
        return Err("an allocation or a free was refused");
    }
    Ok((DONE.load(Ordering::Relaxed), took))
}

// ---- A cache for each CPU, in front of a heap behind a lock ---------------------------

fn with_caches(kind: Work) -> Result<usize, &'static str> {
    let mut memory = vec![0u128; REGION / 16];
    // SAFETY: the region is the test's, and outlives the allocator.
    let tlsf = unsafe { Tlsf::new(memory.as_mut_ptr() as usize, REGION) };
    let central = Box::into_raw(Box::new(SpinLock::new(Central::new(tlsf, BATCH, 0))));
    CENTRAL.store(central, Ordering::Release);
    for cache in &CACHES {
        cache.store(Box::into_raw(Box::new(Cache::new(BATCH, LIMIT))), Ordering::Release);
    }

    fn allocate(cpu: usize, size: usize) -> Option<usize> {
        with_cache(cpu, |cache, central| cache.allocate(central, size, 8))
    }
    fn free(cpu: usize, addr: usize, size: usize) {
        // SAFETY: the block came from a cache's `allocate` with this size, and is freed once.
        if with_cache(cpu, |cache, central| unsafe { cache.free(central, addr, size, 8) }).is_err() {
            REFUSED.fetch_add(1, Ordering::Relaxed);
        }
    }
    let measured = match kind {
        Work::Churn => measure(kind, |cpu| churn(cpu, |size| allocate(cpu, size), |addr, size| free(cpu, addr, size))),
        Work::Pass => measure(kind, |cpu| pass(cpu, |size| allocate(cpu, size), |addr, size| free(cpu, addr, size))),
    };

    let (mut cached, mut cached_bytes) = (0, 0);
    for cache in &CACHES {
        // SAFETY: made by `Box::into_raw` above, and no CPU is using it any more.
        let cache = unsafe { Box::from_raw(cache.swap(core::ptr::null_mut(), Ordering::AcqRel)) };
        cached += cache.blocks();
        cached_bytes += cache.bytes();
    }
    CENTRAL.store(core::ptr::null_mut(), Ordering::Release);
    // SAFETY: as for the caches.
    let mut central = unsafe { Box::from_raw(central) };
    let (done, took) = measured?;
    let (taken, waited) = central.counts();
    match kind {
        Work::Churn => println!(
            "  a cache for each CPU, as in the smp test:  {done} operations, and the lock was taken {taken} times"
        ),
        Work::Pass => println!(
            "  a cache for each CPU:          {} allocations and as many frees, and the lock was taken {taken} times",
            done / 2
        ),
    }
    let heap = central.get_mut().heap.stats();
    println!("    the heap behind the lock made {} allocations and frees", heap.allocations + heap.frees);
    println!("    when the work ended the caches held {cached} blocks, of {cached_bytes} bytes in all");
    println!("  this run: {waited} of those waited for another CPU; {took} ms");
    central.get_mut().heap.check()?;
    drop(memory);
    Ok(taken)
}

// ---- A heap of pages for each CPU, and no lock ----------------------------------------

/// Returns how many blocks were freed by a CPU that didn't own their page.
fn with_heaps(kind: Work, retain: usize) -> Result<usize, &'static str> {
    let mut memory = vec![0u128; ARENA / 16];
    // SAFETY: the region is the test's, and outlives the arena and the heaps: both are
    // freed below, before it. The `'static` is that promise.
    let arena: &'static Arena = Box::leak(Box::new(unsafe { Arena::new(memory.as_mut_ptr() as usize, ARENA) }));
    for heap in &HEAPS {
        heap.store(Box::into_raw(Box::new(arena.heap(retain))), Ordering::Release);
    }

    // The size is not passed on: the page knows it.
    fn free(cpu: usize, addr: usize) {
        // SAFETY: the block came from a heap of this arena, and is freed once.
        if with_heap(cpu, |heap| unsafe { heap.free(addr) }).is_err() {
            REFUSED.fetch_add(1, Ordering::Relaxed);
        }
    }
    let measured = match kind {
        Work::Churn => {
            measure(kind, |cpu| churn(cpu, |size| with_heap(cpu, |heap| heap.allocate(size, 8)), |addr, _| free(cpu, addr)))
        }
        Work::Pass => measure(kind, |cpu| pass(cpu, |size| with_heap(cpu, |heap| heap.allocate(size, 8)), |addr, _| free(cpu, addr))),
    };
    // The blocks freed by other CPUs are on their pages' lists. Each CPU brings its own home.
    let held_before: usize = (0..CPUS).map(|cpu| with_heap(cpu, |heap| heap.pages())).sum();
    cpus::run(|cpu| with_heap(cpu, |heap| heap.collect_all()));

    let mut all = Stats::default();
    let (mut out, mut pages) = (0, 0);
    let mut checked = Ok(0);
    for heap in &HEAPS {
        // SAFETY: made by `Box::into_raw` above, and no CPU is using it any more.
        let heap = unsafe { Box::from_raw(heap.swap(core::ptr::null_mut(), Ordering::AcqRel)) };
        let stats = heap.stats;
        all.allocations += stats.allocations;
        all.local_frees += stats.local_frees;
        all.remote_frees += stats.remote_frees;
        all.retries += stats.retries;
        all.generic += stats.generic;
        all.collections += stats.collections;
        all.collected += stats.collected;
        all.claimed += stats.claimed;
        all.given_back += stats.given_back;
        out += heap.used();
        pages += heap.pages();
        checked = checked.and(heap.check());
    }
    let (_, most) = arena.owned();
    let (pops, stack_retries) = arena.stack_counts();
    // SAFETY: made by `Box::leak` above; the heaps that borrowed it are gone.
    drop(unsafe { Box::from_raw(arena as *const Arena as *mut Arena) });
    drop(memory);
    let (done, took) = measured?;
    checked?;

    match (kind, retain) {
        (Work::Churn, 1) => {
            println!(
                "  a heap of pages for each CPU:              {done} operations, no lock, and {} frees of another \
                 CPU's block",
                all.remote_frees
            );
            println!(
                "    the CPUs took {} pages of {PAGE} bytes from the arena, and gave back {}",
                all.claimed, all.given_back
            );
            println!("    a page's free list ran out {} times in {} allocations", all.generic, all.allocations);
            println!(
                "    when the work ended the CPUs held no blocks, and their heaps held {pages} pages, of {} bytes in all",
                pages * PAGE
            );
            println!(
                "  this run: at most {most} pages held at once; {stack_retries} pushes and pops of the arena's stack \
                 tried again; {took} ms"
            );
        }
        (Work::Churn, _) => {
            println!(
                "  the same, with heaps that keep no empty page: the CPUs took {} pages and gave back {}",
                all.claimed, all.given_back
            );
            println!(
                "  this run: at most {most} pages held at once; {stack_retries} pushes and pops of the arena's stack \
                 tried again; {took} ms"
            );
        }
        (Work::Pass, _) => {
            println!(
                "  a heap of pages for each CPU:  {} allocations and as many frees, no lock, and {} frees of another \
                 CPU's block",
                all.allocations, all.remote_frees
            );
            println!(
                "    when every CPU had collected its pages, {} blocks had come back to the CPU that allocated them, \
                 and {out} were out",
                all.collected
            );
            println!("    the heaps then held {pages} pages, of {} bytes in all", pages * PAGE);
            println!(
                "  this run: {} of the compare-and-swaps were tried again; the owners took their pages' lists {} times",
                all.retries, all.collections
            );
            println!(
                "  this run: the CPUs took {} pages, and held {held_before} when the rounds ended; {took} ms",
                all.claimed
            );
        }
    }
    if pops != all.claimed {
        return Err("the arena's stack was popped more often than pages were taken");
    }
    if out != 0 {
        return Err("blocks were lost: a heap counts blocks as out when every block has been freed");
    }
    if all.allocations != all.local_frees + all.remote_frees || all.remote_frees != all.collected {
        return Err("the heaps' counts of allocations, frees and blocks collected disagree");
    }
    match kind {
        Work::Churn if all.remote_frees != 0 => Err("a CPU freed a block it hadn't allocated"),
        Work::Pass if all.local_frees != 0 => Err("a CPU freed its own block"),
        _ => Ok(all.remote_frees),
    }
}
