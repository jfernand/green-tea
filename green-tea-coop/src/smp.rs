//! The test of allocation from several CPUs, `cargo xtask kernel --kernel smp`.
//!
//! The kernel starts the board's other CPUs, and then all of them do the same things at
//! once: add to a counter without a lock and with one; allocate and free from the kernel's
//! heap, which is behind a lock; and allocate and free through a cache each, in front of a
//! heap of the test's own.
//!
//! Only the boot CPU prints. The others leave what they counted in atomics.

use alloc::boxed::Box;
use alloc::vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use green_tea_bare::fit::tlsf::Tlsf;
use green_tea_bare::fit::trace::Random;
use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::smp::cache::{Cache, Central};
use green_tea_bare::smp::lock::SpinLock;
use green_tea_bare::{Board, Platform, cpus, heap, println};

/// The CPUs `cargo xtask kernel` gives the machine for this test (`-smp 4`).
const CPUS: usize = 4;

/// How many times each CPU adds one to a counter.
const ADDS: usize = 1_000_000;

/// How many allocations and frees each CPU makes, before it frees what it still holds.
const OPERATIONS: usize = 20_000;
/// The most blocks a CPU holds at once.
const HELD: usize = 96;
/// The largest block asked for.
const LARGEST: usize = 512;

/// The caches move 8 blocks at a time and keep at most 16 of a class.
const BATCH: usize = 8;
const LIMIT: usize = 16;

/// The region the caches' heap is given, from the kernel's heap.
const REGION: usize = 1024 * 1024;

static PLAIN: AtomicUsize = AtomicUsize::new(0);
static LOCKED: SpinLock<usize> = SpinLock::new(0);

/// The heap the caches are in front of, made by `test`.
static CENTRAL: AtomicPtr<SpinLock<Central<Tlsf>>> = AtomicPtr::new(core::ptr::null_mut());

/// What the CPUs counted, added up.
static DONE: AtomicUsize = AtomicUsize::new(0); // allocations and frees made
static WRONG: AtomicUsize = AtomicUsize::new(0); // blocks whose bytes had changed
static REFUSED: AtomicUsize = AtomicUsize::new(0); // allocations refused
static REFILLS: AtomicUsize = AtomicUsize::new(0);
static FLUSHES: AtomicUsize = AtomicUsize::new(0);
static CACHED: AtomicUsize = AtomicUsize::new(0); // blocks in the caches when the work ended
static CACHED_BYTES: AtomicUsize = AtomicUsize::new(0); // and their sizes, added up

pub fn test(fb: Framebuffer) {
    match run() {
        Err(e) => println!("FAIL: {e}"),
        Ok((heap_locks, cache_locks)) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: {CPUS} CPUs made {} allocations and frees each way: the heap's lock was taken {heap_locks} \
                 times, and the lock behind a cache per CPU {cache_locks} times",
                DONE.load(Ordering::Relaxed)
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
    #[cfg(target_arch = "aarch64")]
    vectors()?;
    counter()?;
    let heap_locks = kernel_heap()?;
    let cache_locks = caches(0)?;
    caches(4)?;
    Ok((heap_locks, cache_locks))
}

fn ms(ticks: u64) -> u64 {
    ticks / Board::ticks_per_ms()
}

/// Every CPU, the boot CPU included, has the exception vectors installed: without them a
/// fault doesn't report anything.
#[cfg(target_arch = "aarch64")]
fn vectors() -> Result<(), &'static str> {
    static VBAR: [AtomicUsize; CPUS] = [const { AtomicUsize::new(0) }; CPUS];
    cpus::run(|cpu| {
        let vbar: usize;
        // SAFETY: reading a system register of this CPU's.
        unsafe { core::arch::asm!("mrs {}, vbar_el1", out(reg) vbar) };
        VBAR[cpu].store(vbar, Ordering::Relaxed);
    });
    let boot = VBAR[0].load(Ordering::Relaxed);
    if boot == 0 {
        return Err("the boot CPU has no exception vectors (VBAR_EL1 is 0)");
    }
    if VBAR.iter().any(|vbar| vbar.load(Ordering::Relaxed) != boot) {
        return Err("the CPUs don't all have the same exception vectors");
    }
    Ok(())
}

// ---- A counter ------------------------------------------------------------------------

/// Every CPU adds one to two counters, `ADDS` times: one with nothing to keep the CPUs
/// apart, and one inside a lock.
fn counter() -> Result<(), &'static str> {
    cpus::run(|_| {
        for _ in 0..ADDS {
            // A load and a store, as `counter += 1` compiles to. Another CPU's store can
            // come between them, and is then overwritten.
            let seen = PLAIN.load(Ordering::Relaxed);
            PLAIN.store(core::hint::black_box(seen) + 1, Ordering::Relaxed);
        }
    });
    cpus::run(|_| {
        for _ in 0..ADDS {
            let mut counter = LOCKED.lock();
            let seen = *counter;
            *counter = core::hint::black_box(seen) + 1;
        }
    });
    let (plain, locked) = (PLAIN.load(Ordering::Relaxed), LOCKED.with(|counter| *counter));
    let (taken, waited) = LOCKED.counts();
    println!("each CPU adds 1 to a counter {ADDS} times:");
    println!("  with a lock: the counter is {locked}, and the lock was taken {} times", taken - 1);
    println!("  this run: without a lock the counter is {plain}; with one, {waited} acquisitions waited");
    if locked != CPUS * ADDS {
        return Err("additions made under the lock were lost");
    }
    if taken - 1 != CPUS * ADDS {
        return Err("the lock miscounted its acquisitions");
    }
    Ok(())
}

// ---- The workload ---------------------------------------------------------------------

/// What every CPU does to an allocator: `OPERATIONS` allocations and frees, in ramps. It
/// allocates blocks of 1 to `LARGEST` bytes until it holds `HELD`, then frees them in a
/// random order until it holds none, and starts again. At the end it frees what is left.
/// Every byte of a block is set to the CPU's number, and checked before the block is freed.
fn churn(cpu: usize, mut allocate: impl FnMut(usize) -> Option<usize>, mut free: impl FnMut(usize, usize)) {
    let mut random = Random(cpu as u64 + 1);
    let mut held = [(0usize, 0usize); HELD];
    let mut holding = 0;
    let mut done = 0;
    let mut rising = true;
    let release = |(addr, size): (usize, usize), free: &mut dyn FnMut(usize, usize)| {
        // SAFETY: the block is ours until the free below.
        let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
        if bytes.iter().any(|&byte| byte != cpu as u8 + 1) {
            WRONG.fetch_add(1, Ordering::Relaxed);
        }
        free(addr, size);
    };
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
            // SAFETY: the allocator gave us `size` bytes there.
            unsafe { core::ptr::write_bytes(addr as *mut u8, cpu as u8 + 1, size) };
            held[holding] = (addr, size);
            holding += 1;
        } else {
            let which = random.below(holding);
            holding -= 1;
            release(held[which], &mut free);
            held[which] = held[holding];
        }
        done += 1;
    }
    for &block in &held[..holding] {
        release(block, &mut free);
        done += 1;
    }
    DONE.fetch_add(done, Ordering::Relaxed);
}

/// Clear the counts, run `work` on every CPU, and return how many operations were made
/// and how long it took.
fn measure(work: fn(usize)) -> Result<(usize, u64), &'static str> {
    for count in [&DONE, &WRONG, &REFUSED, &REFILLS, &FLUSHES, &CACHED, &CACHED_BYTES] {
        count.store(0, Ordering::Relaxed);
    }
    let start = Board::ticks();
    cpus::run(work);
    let took = ms(Board::ticks() - start);
    if WRONG.load(Ordering::Relaxed) != 0 {
        return Err("a block's bytes changed while a CPU held it: it was handed out twice");
    }
    if REFUSED.load(Ordering::Relaxed) != 0 {
        return Err("an allocation was refused");
    }
    Ok((DONE.load(Ordering::Relaxed), took))
}

// ---- The kernel's heap, behind its lock -----------------------------------------------

/// Every CPU allocates from the kernel's heap, as `Box` and `Vec` do.
fn kernel_heap() -> Result<usize, &'static str> {
    let used = heap::used();
    let (taken, waited) = heap::lock_counts();
    let (done, took) = measure(|cpu| {
        let layout = |size| Layout::from_size_align(size, 8).expect("a small size and a power of two");
        churn(
            cpu,
            // SAFETY: the sizes are 1 or more, and each block is freed once with its own layout.
            |size| Some(unsafe { alloc::alloc::alloc(layout(size)) } as usize).filter(|&addr| addr != 0),
            |addr, size| unsafe { alloc::alloc::dealloc(addr as *mut u8, layout(size)) },
        )
    })?;
    // `heap::used` took the lock once, before the counts were read.
    let (taken, waited) = (heap::lock_counts().0 - taken, heap::lock_counts().1 - waited);
    println!(
        "each CPU makes {OPERATIONS} allocations and frees, of 1 to {LARGEST} bytes, holding up to {HELD}:"
    );
    println!("  the kernel's heap, behind its lock:  {done} operations, and the lock was taken {taken} times");
    println!("  this run: {waited} of those waited for another CPU; {took} ms");
    if taken != done {
        return Err("the heap's lock wasn't taken once for each operation");
    }
    if heap::used() != used {
        return Err("the kernel's heap didn't end as it began");
    }
    heap::with(|heap| heap.check())?;
    Ok(taken)
}

// ---- A cache for each CPU -------------------------------------------------------------

/// Every CPU allocates through a cache of its own, in front of a heap of the test's with
/// a depot that keeps `keep` batches of each class.
fn caches(keep: usize) -> Result<usize, &'static str> {
    let mut memory = vec![0u128; REGION / 16];
    // SAFETY: the region is the test's, and outlives the allocator.
    let tlsf = unsafe { Tlsf::new(memory.as_mut_ptr() as usize, REGION) };
    let central = Box::into_raw(Box::new(SpinLock::new(Central::new(tlsf, BATCH, keep))));
    CENTRAL.store(central, Ordering::Release);

    let measured = measure(|cpu| {
        // SAFETY: `caches` made it before the CPUs were given this work, and frees it after.
        let central = unsafe { &*CENTRAL.load(Ordering::Acquire) };
        let mut cache = Cache::new(BATCH, LIMIT);
        // Both closures use the cache, one at a time.
        let cache_cell = core::cell::RefCell::new(&mut cache);
        churn(
            cpu,
            |size| cache_cell.borrow_mut().allocate(central, size, 8),
            // SAFETY: the block came from `allocate` with this size, and is freed once.
            |addr, size| {
                unsafe { cache_cell.borrow_mut().free(central, addr, size, 8) }.expect("a cache refused a free")
            },
        );
        REFILLS.fetch_add(cache.stats.refills, Ordering::Relaxed);
        FLUSHES.fetch_add(cache.stats.flushes, Ordering::Relaxed);
        CACHED.fetch_add(cache.blocks(), Ordering::Relaxed);
        CACHED_BYTES.fetch_add(cache.bytes(), Ordering::Relaxed);
    });

    CENTRAL.store(core::ptr::null_mut(), Ordering::Release);
    // SAFETY: made by `Box::into_raw` above, and no CPU is using it any more.
    let mut central = unsafe { Box::from_raw(central) };
    let (done, took) = measured?;
    let (taken, waited) = central.counts();
    let central = central.get_mut();
    let (refills, flushes) = (REFILLS.load(Ordering::Relaxed), FLUSHES.load(Ordering::Relaxed));
    let heap_operations = central.heap.stats().allocations + central.heap.stats().frees;
    let cached = CACHED.load(Ordering::Relaxed);
    let cached_bytes = CACHED_BYTES.load(Ordering::Relaxed);

    if keep == 0 {
        println!("  a cache for each CPU:                {done} operations, and the lock was taken {taken} times");
        println!("    {refills} times to fill a cache with {BATCH} blocks, and {flushes} times to take {BATCH} back");
        println!(
            "    when the work ended the CPUs held no blocks, and their caches held {cached}, of {cached_bytes} bytes in all"
        );
        println!("  this run: {waited} of those waited for another CPU; {took} ms");
    } else {
        println!(
            "  the same, with a depot of {keep} batches:  {done} operations, and the lock was taken {taken} times"
        );
        println!(
            "  this run: {waited} of those waited; {took} ms; the depot served {} of the {refills} fills, and the heap \
             made {heap_operations} allocations and frees",
            central.stats.from_depot
        );
    }
    if taken != refills + flushes {
        return Err("the caches' lock wasn't taken once for each batch");
    }
    if keep == 0 && heap_operations != (refills + flushes) * BATCH {
        return Err("the heap behind the caches wasn't asked for a batch at a time");
    }
    central.heap.check()?;
    drop(memory);
    Ok(taken)
}
