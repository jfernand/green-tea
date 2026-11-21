//! The test of the kernel's own allocators, `cargo xtask kernel --kernel slab`.
//!
//! First the page frames: the RAM after the kernel, handed out in blocks of 1 to 1,024
//! pages by a buddy system that keeps its records in a table. The test prints the free
//! blocks by order after each step, frees every other page to show memory that is half
//! free with no two pages together, and runs a random workload.
//!
//! Then a slab cache of 200-byte objects on top of the frames: when objects are
//! constructed and when they aren't, what a freed slab holds until it is reaped, and how
//! few objects it takes to keep a page.
//!
//! Last the cache the kernel uses itself, for its tasks' records: a thousand tasks, one
//! after another, and as many at once as there are stacks for.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use green_tea_bare::kmem::frames::{self, MAX_ORDER, ORDERS};
use green_tea_bare::kmem::slab::{self, Cache, HEADER};
use green_tea_bare::kmem::{self, PAGE, with_frames};
use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::runtime::{run, spawn};
use green_tea_bare::screen::Screen;
use green_tea_bare::task::{self, yield_now};
use green_tea_bare::{Global, println, stack};

/// The free blocks of each order, as a row of a table.
type Free = [usize; ORDERS];

pub fn test(fb: Framebuffer) {
    let verdict = boot_state().and_then(|boot| {
        blocks(boot)?;
        let merges = every_other_page(boot)?;
        workload(boot)?;
        let constructed = objects(boot)?;
        pinned(boot)?;
        mistakes()?;
        let (per_slab, at_once) = tasks(boot)?;
        Ok((merges, constructed, per_slab, at_once))
    });
    match verdict {
        Err(e) => println!("FAIL: {e}"),
        Ok((merges, constructed, per_slab, at_once)) => {
            Screen::new(fb).finish(at_once);
            println!(
                "PASS: {} pages freed one by one merged {merges} times into the blocks they came from; 40 objects \
                 allocated twice were constructed {constructed} times; {TASKS} tasks took {per_slab} stacks from \
                 the pool, and {at_once} ran at once",
                kmem::PAGES - kmem::TABLE_PAGES
            );
        }
    }
}

fn row(label: &str, free: Free, pages: usize) {
    let mut text = String::new();
    for count in free {
        let _ = write!(text, "{count:5}");
    }
    println!("  {label:24}{text} {pages:10}");
}

fn show(label: &str) -> Free {
    let (free, pages) = with_frames(|frames| (frames.free_blocks(), frames.free_pages()));
    row(label, free, pages);
    free
}

/// A page's number: its place in the region.
fn page_of(addr: usize) -> usize {
    (addr - kmem::region().start) / PAGE
}

/// The page allocator as the kernel starts with it. Nothing has allocated from it yet:
/// the test runs before the first task.
fn boot_state() -> Result<Free, String> {
    let region = kmem::region();
    println!(
        "page frames: {} of {PAGE} bytes, the {} MiB of RAM from {:#x}, where the kernel ends",
        kmem::PAGES,
        kmem::REGION >> 20,
        region.start
    );
    println!(
        "their table: {} records of {} bytes, in the first {} pages",
        kmem::PAGES,
        size_of::<kmem::Frame>(),
        kmem::TABLE_PAGES
    );
    println!("  {:24}    0    1    2    3    4    5    6    7    8    9   10 pages free", "free blocks of order");
    let boot = show("at boot");
    with_frames(|frames| frames.check())?;
    // 12 pages of table leave 4 pages up to 16, then 16, 32, ... 512, and three of 1,024.
    let mut expected = [0, 0, 1, 0, 1, 1, 1, 1, 1, 1, 3];
    expected[MAX_ORDER] = 3;
    if boot != expected || kmem::TABLE_PAGES != 12 {
        return Err("the page allocator didn't start with the region in its largest blocks".into());
    }
    Ok(boot)
}

/// Three allocations and their frees, with the lists after each.
fn blocks(boot: Free) -> Result<(), String> {
    let allocate = |order| with_frames(|frames| frames.allocate(order)).ok_or("the page allocator refused a block");
    let a = allocate(0)?;
    show(&format!("a = allocate(0): page {}", page_of(a)));
    let b = allocate(0)?;
    show(&format!("b = allocate(0): page {}", page_of(b)));
    let c = allocate(3)?;
    let split = show(&format!("c = allocate(3): page {}", page_of(c)));
    if (page_of(a), page_of(b), page_of(c)) != (12, 13, 16) || split != [0, 1, 0, 1, 0, 1, 1, 1, 1, 1, 3] {
        return Err("a block wasn't halved as it should be".into());
    }
    if with_frames(|frames| frames.order_of(c)) != Some(3) {
        return Err("the table doesn't know the order of an allocated block".into());
    }
    for (name, addr) in [("a", a), ("b", b), ("c", c)] {
        let order = with_frames(|frames| frames.free(addr)).map_err(|e| format!("free({name}): {e:?}"))?;
        show(&format!("free({name}), of order {order}"));
    }
    let stats = with_frames(|frames| frames.stats());
    println!("  {} blocks halved, {} pairs of buddies merged", stats.splits, stats.merges);
    let again = with_frames(|frames| frames.free(c));
    println!("  free(c) again: {again:?}");
    with_frames(|frames| frames.check())?;
    if with_frames(|frames| frames.free_blocks()) != boot || (stats.splits, stats.merges) != (3, 3) {
        return Err("three blocks freed didn't merge back into the blocks they were split from".into());
    }
    if again != Err(frames::FreeError::NotAllocated) {
        return Err("the page allocator took a block back twice".into());
    }
    Ok(())
}

/// Every page allocated on its own, written, and every other one freed: half the memory
/// free, and no block of two pages. Returns how many merges freeing the rest took.
fn every_other_page(boot: Free) -> Result<usize, String> {
    let before = with_frames(|frames| frames.stats());
    let mut pages = Vec::new();
    while let Some(page) = with_frames(|frames| frames.allocate(0)) {
        // The page's own number at each end of it: this is RAM, and nobody else's.
        unsafe {
            (page as *mut usize).write(page);
            ((page + PAGE - size_of::<usize>()) as *mut usize).write(!page);
        }
        pages.push(page);
    }
    println!("every page allocated on its own, {} of them, and written:", pages.len());
    show("none left");
    for page in pages.iter().step_by(2) {
        with_frames(|frames| frames.free(*page)).map_err(|e| format!("a page couldn't be freed: {e:?}"))?;
    }
    let half = show("every other one freed");
    let pair = with_frames(|frames| frames.allocate(1));
    println!(
        "  {} pages free, {} in use; a request for two pages together: {}",
        with_frames(|frames| frames.free_pages()),
        pages.len() - pages.len() / 2,
        if pair.is_none() { "refused" } else { "granted" }
    );
    if pages.len() != kmem::PAGES - kmem::TABLE_PAGES || half[0] != pages.len() / 2 || half[1..] != [0; MAX_ORDER] {
        return Err("every other page free should be that many blocks of one page".into());
    }
    if pair.is_some() {
        return Err("a block of two pages was found where no two free pages are buddies".into());
    }
    // The pages still held read back what was written to them.
    let intact = |page: usize| unsafe {
        (page as *const usize).read() == page && ((page + PAGE - size_of::<usize>()) as *const usize).read() == !page
    };
    if !pages.iter().skip(1).step_by(2).all(|page| intact(*page)) {
        return Err("a page didn't read back what was written to it".into());
    }
    for page in pages.iter().skip(1).step_by(2) {
        with_frames(|frames| frames.free(*page)).map_err(|e| format!("a page couldn't be freed: {e:?}"))?;
    }
    let after = with_frames(|frames| frames.stats());
    let (splits, merges) = (after.splits - before.splits, after.merges - before.merges);
    show("the rest freed");
    println!("  {splits} blocks halved on the way down, {merges} pairs merged on the way back");
    with_frames(|frames| frames.check())?;
    if with_frames(|frames| frames.free_blocks()) != boot || splits != merges {
        return Err("the pages didn't merge back into the blocks they were split from".into());
    }
    Ok(merges)
}

/// The pseudo-random numbers of the `fit` test: xorshift, from a fixed seed.
struct Random(u64);

impl Random {
    fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as usize
    }
}

const OPERATIONS: usize = 20_000;
const HELD: usize = 300;

/// Blocks of 1 to 32 pages, allocated and freed at random.
fn workload(boot: Free) -> Result<(), String> {
    let before = with_frames(|frames| frames.stats());
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let mut held: Vec<usize> = Vec::with_capacity(HELD);
    let (mut most, mut at_most) = (0, ([0; ORDERS], 0, 0));
    for _ in 0..OPERATIONS {
        if held.len() < HELD && !random.next().is_multiple_of(3) {
            let order = random.next() % 6;
            if let Some(block) = with_frames(|frames| frames.allocate(order)) {
                held.push(block);
            }
        } else if !held.is_empty() {
            let block = held.swap_remove(random.next() % held.len());
            with_frames(|frames| frames.free(block)).map_err(|e| format!("a block couldn't be freed: {e:?}"))?;
        }
        let used = with_frames(|frames| frames.used_pages()) - kmem::TABLE_PAGES;
        if used > most {
            most = used;
            at_most = with_frames(|frames| (frames.free_blocks(), frames.free_pages(), held.len()));
        }
    }
    println!("{OPERATIONS} allocations and frees at random, of 1 to 32 pages, up to {HELD} blocks at once:");
    let (free, pages, blocks) = at_most;
    row("at the most in use", free, pages);
    println!("  then: {most} pages in {blocks} blocks");
    let end = show("at the end");
    let (in_use, largest) = with_frames(|frames| (frames.used_pages() - kmem::TABLE_PAGES, frames.largest_free()));
    println!(
        "  then: {in_use} pages in {} blocks; the largest free block is of order {}",
        held.len(),
        largest.map_or(String::from("none"), |order| format!("{order}"))
    );
    // Free pages aren't a free block: ask for the order above the largest there is.
    let above = largest.map_or(0, |order| order + 1);
    let large = with_frames(|frames| frames.allocate(above));
    println!(
        "  a request for {} pages together (order {above}), with {} free: {}",
        1 << above,
        with_frames(|frames| frames.free_pages()),
        if large.is_none() { "refused" } else { "granted" }
    );
    if large.is_some() || above > MAX_ORDER {
        return Err("a block was found of an order that has no free block".into());
    }
    with_frames(|frames| frames.check())?;
    for block in held {
        with_frames(|frames| frames.free(block)).map_err(|e| format!("a block couldn't be freed: {e:?}"))?;
    }
    show("everything freed");
    let after = with_frames(|frames| frames.stats());
    println!(
        "  {} allocations, {} blocks halved, {} pairs merged",
        after.allocations - before.allocations,
        after.splits - before.splits,
        after.merges - before.merges
    );
    with_frames(|frames| frames.check())?;
    if with_frames(|frames| frames.free_blocks()) != boot
        || after.splits - before.splits != after.merges - before.merges
    {
        return Err("the workload's blocks didn't merge back into the blocks they were split from".into());
    }
    if end == boot || most == 0 {
        return Err("the workload held nothing".into());
    }
    Ok(())
}

// ---- A cache of objects -----------------------------------------------------------------

/// The size of the test's objects: the size of Bonwick's example.
const OBJECT: usize = 200;
/// What the constructor leaves in an object's first word and in its last.
const MARK: usize = 0x636f_6e73_7472_7563;

/// How many objects the constructor has been called on, and the destructor.
static CONSTRUCTED: Global<usize> = Global::new(0);
static DESTROYED: Global<usize> = Global::new(0);

fn construct(object: *mut u8) -> bool {
    CONSTRUCTED.set(CONSTRUCTED.get() + 1);
    unsafe {
        (object as *mut usize).write(MARK);
        (object.add(OBJECT - size_of::<usize>()) as *mut usize).write(MARK);
    }
    true
}

fn destroy(object: *mut u8) {
    DESTROYED.set(DESTROYED.get() + 1);
    unsafe { (object as *mut usize).write(0) };
}

fn constructed(object: usize) -> bool {
    unsafe {
        (object as *const usize).read() == MARK
            && ((object + OBJECT - size_of::<usize>()) as *const usize).read() == MARK
    }
}

fn object_cache() -> Cache {
    Cache::new("objects", OBJECT, 8, Some(construct), Some(destroy))
}

fn allocate(cache: &mut Cache) -> Result<usize, String> {
    let object = with_frames(|frames| cache.allocate(frames)).ok_or("the cache refused an object")?;
    if !constructed(object) {
        return Err("the cache handed out an object that isn't in its constructed state".into());
    }
    Ok(object)
}

fn free(cache: &mut Cache, object: usize) -> Result<(), String> {
    // SAFETY: every address the test frees came from `allocate` on this cache.
    unsafe { cache.free(object) }.map_err(|e| format!("an object couldn't be freed: {e:?}"))
}

/// One line of the cache's state: its slabs by list, its objects, its pages, and what the
/// constructor and the destructor have done.
fn state(label: &str, cache: &Cache) -> ((usize, usize, usize), usize, usize) {
    let ((full, partial, empty), stats) = (cache.slabs(), cache.stats());
    let free_pages = with_frames(|frames| frames.free_pages());
    println!(
        "  {label:26}{full:5}{partial:8}{empty:6}{:8}{:12}{:10}{:6}{free_pages:11}",
        cache.in_use(),
        stats.constructed,
        stats.destroyed,
        cache.pages()
    );
    (cache.slabs(), stats.constructed, stats.destroyed)
}

/// Objects allocated, freed, allocated again, and reaped. Returns how many times the
/// constructor ran for forty objects allocated twice.
fn objects(boot: Free) -> Result<usize, String> {
    let mut cache = object_cache();
    println!(
        "a cache of objects of {OBJECT} bytes: a buffer is {} bytes, the object and then a link",
        cache.buffer_size()
    );
    println!(
        "a slab is a page: {} buffers, a record of {HEADER} bytes at the end, and {} bytes to spare",
        cache.per_slab(),
        cache.spare()
    );
    println!(
        "  {:26}{:>5}{:>8}{:>6}{:>8}{:>12}{:>10}{:>6}{:>11}",
        "slabs:", "full", "partial", "empty", "objects", "constructed", "destroyed", "pages", "pages free"
    );
    state("a new cache", &cache);
    let mut held = Vec::new();
    held.push(allocate(&mut cache)?);
    let one = state("1 object allocated", &cache);
    for _ in 1..19 {
        held.push(allocate(&mut cache)?);
    }
    let slab = state("19 objects", &cache);
    for _ in 19..40 {
        held.push(allocate(&mut cache)?);
    }
    let forty = state("40 objects", &cache);
    for object in &held {
        free(&mut cache, *object)?;
    }
    let freed = state("all 40 freed", &cache);
    if !held.iter().all(|object| constructed(*object)) {
        return Err("a free overwrote an object's constructed state".into());
    }
    let mut again = Vec::new();
    for _ in 0..40 {
        again.push(allocate(&mut cache)?);
    }
    let twice = state("40 objects again", &cache);
    for object in &again {
        free(&mut cache, *object)?;
    }
    let reaped = with_frames(|frames| cache.reap(frames));
    let end = state("all freed, and reaped", &cache);
    cache.check()?;
    with_frames(|frames| frames.check())?;
    // What the constructor did, and when: once per object of each new slab, and never again.
    let right = one == ((0, 1, 0), 19, 0)
        && slab == ((1, 0, 0), 19, 0)
        && forty == ((2, 1, 0), 57, 0)
        && freed == ((0, 0, 3), 57, 0)
        && twice == ((2, 1, 0), 57, 0)
        && (reaped, end) == (3, ((0, 0, 0), 57, 57));
    if !right || (CONSTRUCTED.get(), DESTROYED.get()) != (57, 57) {
        return Err("objects weren't constructed once for each slab, or weren't kept constructed".into());
    }
    if with_frames(|frames| frames.free_blocks()) != boot {
        return Err("a reaped cache kept a page".into());
    }

    let twice = CONSTRUCTED.get();

    // Colours: where the first buffer of each new slab starts in its page.
    let mut cache = object_cache();
    let mut offsets = String::new();
    let mut held = Vec::new();
    for index in 0..16 * cache.per_slab() {
        let object = allocate(&mut cache)?;
        if index.is_multiple_of(cache.per_slab()) {
            let _ = write!(offsets, " {}", object % PAGE);
        }
        held.push(object);
    }
    println!("the first buffer of each of 16 new slabs, as an offset in its page ({} colours):", cache.colours());
    println!(" {offsets}");
    for object in held {
        free(&mut cache, object)?;
    }
    with_frames(|frames| cache.reap(frames));
    if offsets != " 0 8 16 24 32 40 48 56 64 72 80 88 96 104 0 8" || cache.colours() != 14 {
        return Err("successive slabs didn't start their buffers at successive offsets".into());
    }
    Ok(twice)
}

/// Ten slabs' worth of objects, of which ten stay: one in each slab, or all in one.
fn pinned(boot: Free) -> Result<(), String> {
    let mut held = [0, 0];
    for (which, spread) in [true, false].into_iter().enumerate() {
        let mut cache = object_cache();
        let per_slab = cache.per_slab();
        let mut objects = Vec::new();
        for _ in 0..10 * per_slab {
            objects.push(allocate(&mut cache)?);
        }
        // Keep ten: the first of each slab, or the first ten of the first slab.
        let mut kept = Vec::new();
        for (index, object) in objects.into_iter().enumerate() {
            if if spread { index.is_multiple_of(per_slab) } else { index < 10 } {
                kept.push(object);
            } else {
                free(&mut cache, object)?;
            }
        }
        let (_, partial, empty) = cache.slabs();
        let reaped = with_frames(|frames| cache.reap(frames));
        held[which] = cache.pages();
        println!(
            "190 objects in 10 slabs, all freed but 10, {}: slabs {partial} partly used, {empty} empty",
            if spread { "one in each slab" } else { "all in one slab" }
        );
        println!("  reaped, the cache gives back {reaped} pages and keeps {}", cache.pages());
        cache.check()?;
        for object in kept {
            free(&mut cache, object)?;
        }
        with_frames(|frames| cache.reap(frames));
    }
    if held != [10, 1] {
        return Err("a slab with an object in use was reaped, or one with none was kept".into());
    }
    if with_frames(|frames| frames.free_blocks()) != boot {
        return Err("the caches kept a page".into());
    }
    Ok(())
}

/// A second free, and a free of an address inside an object.
fn mistakes() -> Result<(), String> {
    let mut cache = object_cache();
    let object = allocate(&mut cache)?;
    // SAFETY: the address is in a slab of this cache, which is what the cache can check.
    let inside = unsafe { cache.free(object + 8) };
    free(&mut cache, object)?;
    let twice = unsafe { cache.free(object) };
    println!("an address inside an object, freed: {inside:?}; an object freed a second time: {twice:?}");
    with_frames(|frames| cache.reap(frames));
    if inside != Err(slab::FreeError::NotAnObject) || twice != Err(slab::FreeError::NotAllocated) {
        return Err("the cache took back what it hadn't handed out".into());
    }
    Ok(())
}

// ---- The kernel's own cache: task records -----------------------------------------------

/// How many tasks the first part runs, one after another.
const TASKS: usize = 1000;

/// What a task in `run` saw of the cache of task records, for the code after `run`.
#[derive(Clone, Copy)]
struct Seen {
    slabs: (usize, usize, usize),
    in_use: usize,
    stats: slab::Stats,
    stacks_free: usize,
    pages_free: usize,
}

static SEEN: Global<Option<Seen>> = Global::new(None);
static RAN: Global<usize> = Global::new(0);
/// The lowest and highest address of a local variable in any of the tasks.
static LOCALS: Global<(usize, usize)> = Global::new((usize::MAX, 0));

fn look() {
    let (slabs, in_use, stats) = task::cache(|cache| (cache.slabs(), cache.in_use(), cache.stats()));
    let seen = Seen {
        slabs,
        in_use,
        stats,
        stacks_free: stack::free_slots(),
        pages_free: with_frames(|frames| frames.free_pages()),
    };
    SEEN.set(Some(seen));
}

/// Returns how many records a slab holds, and how many tasks ran at once.
fn tasks(boot: Free) -> Result<(usize, usize), String> {
    let (size, buffer, per_slab) = task::cache(|cache| (cache.object_size(), cache.buffer_size(), cache.per_slab()));
    let pages_free = with_frames(|frames| frames.free_pages());
    println!(
        "the kernel's cache of task records: a record is {size} bytes, a buffer {buffer}, and a slab holds {per_slab}"
    );
    println!("  a constructed record has a stack, of which the pool has {}", stack::SLOTS);
    if stack::free_slots() != stack::SLOTS || task::cache(|cache| cache.pages()) != 0 {
        return Err("the cache of task records wasn't empty before the first task".into());
    }

    run(
        || {
            for _ in 0..TASKS {
                spawn(|| {
                    let local = 0u8;
                    let addr = &raw const local as usize;
                    let (low, high) = LOCALS.get();
                    LOCALS.set((low.min(addr), high.max(addr)));
                    RAN.set(RAN.get() + 1);
                });
                yield_now(); // the new task runs, and finishes, before the next is spawned
            }
            look();
        },
        |_| {},
    );
    let seen = SEEN.get().ok_or("the first task didn't finish")?;
    let (low, high) = LOCALS.get();
    println!("{} tasks, one after another, every one with its locals at {low:#x}:", RAN.get());
    println!(
        "  {} records allocated, {} constructed, in {} slab; {} stacks taken from the pool",
        seen.stats.allocations,
        seen.stats.constructed,
        seen.stats.slabs_created,
        stack::SLOTS - seen.stacks_free
    );
    let after = task::cache(|cache| cache.stats());
    println!(
        "  the last task finished, the scheduler reaps: {} slab and {} records destroyed, and the pool has {} stacks",
        after.slabs_destroyed,
        after.destroyed,
        stack::free_slots()
    );
    if RAN.get() != TASKS || low != high {
        return Err("the tasks didn't all run on the same stack".into());
    }
    if (seen.stats.allocations, seen.stats.constructed, seen.stats.slabs_created) != (TASKS + 1, per_slab, 1)
        || seen.stacks_free != stack::SLOTS - per_slab
        || seen.pages_free != pages_free - 1
    {
        return Err("a task's record was constructed more than once, or took a stack of its own".into());
    }
    if (after.slabs_destroyed, after.destroyed) != (1, per_slab) || stack::free_slots() != stack::SLOTS {
        return Err("the scheduler didn't give the records' stacks back when the tasks had finished".into());
    }

    // As many tasks at once as whole slabs of records have stacks for.
    let at_once = stack::SLOTS / per_slab * per_slab;
    RAN.set(0);
    SEEN.set(None);
    run(
        move || {
            for _ in 1..at_once {
                spawn(|| {
                    yield_now(); // so that none finishes before the last is spawned
                    RAN.set(RAN.get() + 1);
                });
            }
            look();
        },
        |_| {},
    );
    let seen = SEEN.get().ok_or("the first task didn't finish")?;
    println!("{at_once} tasks at once:");
    println!(
        "  {} slabs, all full, in {} pages; the pool has {} stacks left, too few for another slab",
        seen.slabs.0,
        pages_free - seen.pages_free,
        seen.stacks_free
    );
    let slabs = at_once / per_slab;
    if RAN.get() != at_once - 1 || seen.slabs != (slabs, 0, 0) || seen.in_use != at_once {
        return Err("the tasks weren't all alive at once".into());
    }
    if seen.stacks_free != stack::SLOTS - at_once
        || seen.stacks_free >= per_slab
        || pages_free - seen.pages_free != slabs
    {
        return Err("a slab of task records didn't hold a stack for each record".into());
    }
    task::cache(|cache| cache.check())?;
    with_frames(|frames| frames.check())?;
    let stats = task::cache(|cache| cache.stats());
    println!(
        "  afterwards: {} slabs created and {} destroyed, {} records constructed and {} destroyed",
        stats.slabs_created, stats.slabs_destroyed, stats.constructed, stats.destroyed
    );
    println!("  the pool has {} of {} stacks, and the page frames are as at boot", stack::free_slots(), stack::SLOTS);
    if stack::free_slots() != stack::SLOTS || with_frames(|frames| frames.free_blocks()) != boot {
        return Err("the tasks left a stack or a page behind".into());
    }
    if stats.slabs_created != stats.slabs_destroyed || stats.constructed != stats.destroyed {
        return Err("a task record was constructed and never destroyed".into());
    }
    Ok((per_slab, at_once))
}
