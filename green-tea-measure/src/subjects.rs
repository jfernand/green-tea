//! Every single-threaded allocator of the workspace behind one interface, so that one
//! loop can replay a trace against each.
//!
//! A [`Subject`] owns its memory. Most are one allocator over one region. Four are an
//! allocator for small blocks, in twelve size classes, with TLSF or the page-frame
//! allocator behind it for everything else: those are the designs of the chapters on
//! pools, slabs, per-CPU caches and sharded free lists, each used here from one thread.

use green_tea_fit::buddy::Buddy;
use green_tea_fit::hardened::{ALL, Hardened};
use green_tea_fit::tlsf::Tlsf;
use green_tea_fit::{Fit, Heap, Order, Policy};
use green_tea_kmem::frames::{Frame, Frames};
use green_tea_kmem::slab::Cache as Slab;
use green_tea_kmem::{PAGE, PageSource};
use green_tea_pool::{FreeList, Pool};
use green_tea_shard::Arena;
use green_tea_smp::cache::{CLASSES, Cache, Central, class};
use green_tea_smp::lock::SpinLock;

/// An allocator under measurement.
pub trait Subject {
    /// A block of at least `size` bytes at a multiple of `align`, or `None`.
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize>;

    /// Give back a block, with the size and alignment it was asked for. Some allocators
    /// need them and some don't; the replay knows them either way.
    ///
    /// # Safety
    /// `addr` must have come from `allocate` on this subject with this `size` and `align`,
    /// and not have been freed since.
    unsafe fn free(&mut self, addr: usize, size: usize, align: usize);

    /// Bytes in allocated blocks with whatever the allocator adds to each, for the
    /// allocators that keep that number.
    fn used(&self) -> Option<usize>;

    /// How much memory the allocator has needed: for one region, the distance from its
    /// start to the highest address a block has had; for several regions, the sum; for
    /// page frames, the most pages held at once.
    fn footprint(&self) -> usize;

    /// For the allocators that search: the free blocks looked at so far, by allocations
    /// and by frees together, and the most that any one call looked at.
    fn searched(&self) -> Option<(usize, usize)>;

    /// Check everything the allocator relies on.
    fn check(&self) -> Result<(), &'static str>;

    /// Start again, on the same memory, as a new allocator.
    fn reset(&mut self);
}

/// The names [`build`] knows, in the order the tables print them.
pub const NAMES: [&str; 15] = [
    "bump",
    "first-lifo",
    "first-addr",
    "next-lifo",
    "next-addr",
    "best-lifo",
    "best-addr",
    "buddy",
    "tlsf",
    "hardened",
    "pools",
    "slabs",
    "caches",
    "shards",
    "null",
];

/// The seed of the hardened allocator's secrets and choices.
const SEED: u64 = 0x2545_f491_4f6c_dd1d;
/// Blocks a cache moves at a time, the most it holds of a class, and the batches the depot keeps.
const BATCH: usize = 8;
const LIMIT: usize = 16;
const KEEP: usize = 2;
/// Pages of a class that a sharded heap keeps when they are empty.
const RETAIN: usize = 1;

/// A zeroed region of `len` bytes. The operating system maps its pages when they are
/// first written.
///
/// The region starts at a multiple of 4,096. Where `Vec` puts its buffer is the C
/// library's choice, and differs from one allocation to the next; a block aligned to 64
/// bytes is placed by its address, so a region that started wherever the buffer did would
/// give a footprint that depended on it.
pub struct Memory {
    /// Held so that the region lives as long as the `Memory` does.
    _buffer: Vec<u128>,
    base: usize,
    len: usize,
}

impl Memory {
    pub fn new(len: usize) -> Memory {
        let len = len.next_multiple_of(16);
        let mut buffer = vec![0u128; (len + 2 * PAGE) / 16];
        let base = (buffer.as_mut_ptr() as usize).next_multiple_of(PAGE);
        Memory { _buffer: buffer, base, len }
    }

    pub fn base(&self) -> usize {
        self.base
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The subject called `name`. `region` is the bytes each of its regions gets, and `total`
/// the bytes the trace allocates in all, which is what a subject that never frees needs.
pub fn build(name: &str, region: usize, total: usize) -> Option<Box<dyn Subject>> {
    Some(match name {
        // Twice the bytes: a block aligned to 64 can waste 63 before it.
        "bump" => Box::new(Bump::new(2 * total + 65536)),
        "first-lifo" => fit(region, Policy::FirstFit, Order::Lifo),
        "first-addr" => fit(region, Policy::FirstFit, Order::Address),
        "next-lifo" => fit(region, Policy::NextFit, Order::Lifo),
        "next-addr" => fit(region, Policy::NextFit, Order::Address),
        "best-lifo" => fit(region, Policy::BestFit, Order::Lifo),
        "best-addr" => fit(region, Policy::BestFit, Order::Address),
        // SAFETY (the next three): each region is the subject's own, in a `Memory` it keeps.
        "buddy" => Box::new(One::new(region, |base, len| unsafe { Buddy::new(base, len) })),
        "tlsf" => Box::new(One::new(region, |base, len| unsafe { Tlsf::new(base, len) })),
        "hardened" => Box::new(One::new(region, |base, len| unsafe { Hardened::<ALL>::new(base, len, SEED) })),
        "pools" => Box::new(Pools::new(region)),
        "slabs" => Box::new(Slabs::new(region)),
        "caches" => Box::new(Caches::new(region)),
        "shards" => Box::new(Shards::new(region)),
        "null" => Box::new(Null { next: 0 }),
        _ => return None,
    })
}

fn fit(region: usize, policy: Policy, order: Order) -> Box<dyn Subject> {
    // SAFETY: the region is the subject's own.
    Box::new(One::new(region, move |base, len| unsafe { Fit::new(base, len, policy, order) }))
}

/// One allocator of green-tea-fit over one region.
struct One<H, F> {
    memory: Memory,
    make: F,
    heap: H,
}

impl<H: Heap, F: Fn(usize, usize) -> H> One<H, F> {
    fn new(region: usize, make: F) -> One<H, F> {
        let memory = Memory::new(region);
        let heap = make(memory.base(), memory.len());
        One { memory, make, heap }
    }
}

impl<H: Heap, F: Fn(usize, usize) -> H> Subject for One<H, F> {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        self.heap.allocate(size, align)
    }

    unsafe fn free(&mut self, addr: usize, _size: usize, _align: usize) {
        // SAFETY: the caller's promise.
        unsafe { self.heap.free(addr) }.expect("the allocator refused a block it had handed out");
    }

    fn used(&self) -> Option<usize> {
        Some(self.heap.used())
    }

    fn footprint(&self) -> usize {
        self.heap.footprint()
    }

    fn searched(&self) -> Option<(usize, usize)> {
        let stats = self.heap.stats();
        Some((stats.examined + stats.passed, stats.longest_search.max(stats.longest_walk)))
    }

    fn check(&self) -> Result<(), &'static str> {
        self.heap.check()
    }

    fn reset(&mut self) {
        self.heap = (self.make)(self.memory.base(), self.memory.len());
    }
}

/// No allocator. `allocate` returns a number that no other live block has, and `free`
/// does nothing. A replay against it costs what the replay costs.
struct Null {
    next: usize,
}

impl Subject for Null {
    fn allocate(&mut self, _size: usize, _align: usize) -> Option<usize> {
        self.next += 16;
        Some(self.next)
    }

    unsafe fn free(&mut self, _addr: usize, _size: usize, _align: usize) {}

    fn used(&self) -> Option<usize> {
        None
    }

    fn footprint(&self) -> usize {
        0
    }

    fn searched(&self) -> Option<(usize, usize)> {
        None
    }

    fn check(&self) -> Result<(), &'static str> {
        Ok(())
    }

    fn reset(&mut self) {
        self.next = 0;
    }
}

/// A bump pointer: every block goes above the last, and nothing is ever freed.
struct Bump {
    memory: Memory,
    next: usize,
    end: usize,
}

impl Bump {
    fn new(len: usize) -> Bump {
        let memory = Memory::new(len);
        let (next, end) = (memory.base(), memory.base() + memory.len());
        Bump { memory, next, end }
    }
}

impl Subject for Bump {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        let addr = self.next.next_multiple_of(align);
        let end = addr.checked_add(size)?;
        if end > self.end {
            return None;
        }
        self.next = end;
        Some(addr)
    }

    unsafe fn free(&mut self, _addr: usize, _size: usize, _align: usize) {}

    fn used(&self) -> Option<usize> {
        None
    }

    fn footprint(&self) -> usize {
        self.next - (self.end - self.memory.len())
    }

    fn searched(&self) -> Option<(usize, usize)> {
        None
    }

    fn check(&self) -> Result<(), &'static str> {
        Ok(())
    }

    fn reset(&mut self) {
        self.next = self.memory.base();
    }
}

/// TLSF in a region of its own, for what a size-class allocator doesn't serve.
struct Large {
    memory: Memory,
    tlsf: Tlsf,
}

impl Large {
    fn new(region: usize) -> Large {
        let memory = Memory::new(region);
        // SAFETY: the region is ours.
        let tlsf = unsafe { Tlsf::new(memory.base(), memory.len()) };
        Large { memory, tlsf }
    }

    fn reset(&mut self) {
        // SAFETY: as in `new`.
        self.tlsf = unsafe { Tlsf::new(self.memory.base(), self.memory.len()) };
    }
}

/// Twelve pools, one for each size class, each a free list through its free slots.
/// A pool's slots are never used for another size.
struct Pools {
    /// Held so that the region lives as long as the pools do.
    _memory: Memory,
    base: usize,
    /// The bytes each pool has.
    share: usize,
    pools: Vec<FreeList>,
    /// For each pool, the end of the highest slot it has handed out, from the pool's start.
    high: [usize; CLASSES.len()],
    large: Large,
}

impl Pools {
    fn new(region: usize) -> Pools {
        let share = region.next_multiple_of(16);
        let memory = Memory::new(share * CLASSES.len());
        let base = memory.base();
        let pools = Pools::make(base, share);
        Pools { _memory: memory, base, share, pools, high: [0; CLASSES.len()], large: Large::new(region) }
    }

    fn make(base: usize, share: usize) -> Vec<FreeList> {
        let pool = |(i, &size): (usize, &usize)| {
            // SAFETY: the pools' shares of the region don't overlap, and the region is ours.
            unsafe { FreeList::new(base + i * share, size, share / size, 0) }
        };
        CLASSES.iter().enumerate().map(pool).collect()
    }
}

impl Subject for Pools {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        match class(size, align) {
            Some(class) => {
                let slot = self.pools[class].take()?;
                let end = slot + CLASSES[class] - (self.base + class * self.share);
                self.high[class] = self.high[class].max(end);
                Some(slot)
            }
            None => self.large.tlsf.allocate(size, align),
        }
    }

    unsafe fn free(&mut self, addr: usize, size: usize, align: usize) {
        // SAFETY: the caller's promise, and the same class as when it was allocated.
        unsafe {
            match class(size, align) {
                Some(class) => self.pools[class].give(addr).expect("a pool refused a slot"),
                None => self.large.tlsf.free(addr).expect("TLSF refused a block it had handed out"),
            }
        }
    }

    fn used(&self) -> Option<usize> {
        None
    }

    /// The slots each pool has ever handed out, and what TLSF reached.
    fn footprint(&self) -> usize {
        self.high.iter().sum::<usize>() + self.large.tlsf.footprint()
    }

    fn searched(&self) -> Option<(usize, usize)> {
        None
    }

    fn check(&self) -> Result<(), &'static str> {
        self.large.tlsf.check()
    }

    fn reset(&mut self) {
        self.pools = Pools::make(self.base, self.share);
        self.high = [0; CLASSES.len()];
        self.large.reset();
    }
}

/// Page frames, and the most of them that have been allocated at once.
struct Pages {
    frames: Frames<'static>,
    most: usize,
}

impl Pages {
    fn allocate(&mut self, order: usize) -> Option<usize> {
        let addr = self.frames.allocate(order)?;
        self.most = self.most.max(self.frames.used_pages());
        Some(addr)
    }
}

// SAFETY: `Frames` supplies unique page-aligned pages, and `give_page` returns them to that
// same frame allocator.
unsafe impl PageSource for Pages {
    fn take_page(&mut self) -> Option<usize> {
        self.allocate(0)
    }

    unsafe fn give_page(&mut self, page: usize) {
        self.frames.free(page).expect("the frame allocator refused a page a slab gave back");
    }
}

/// What a kernel has: a slab cache for each size class, over page frames from a buddy
/// system. A block larger than the largest class is a power of two of whole pages.
/// A slab whose objects have all been freed is kept until `reap`, which nothing here calls.
struct Slabs {
    memory: Memory,
    /// The frame allocator's table, which is leaked: a subject lives as long as the
    /// program does. It is kept here so that `reset` can use it again.
    table: *mut [Frame],
    /// `None` only inside `reset`, between the old allocator and the new.
    pages: Option<Pages>,
    caches: Vec<Slab>,
}

impl Slabs {
    fn new(region: usize) -> Slabs {
        // Twice the region: every block over a kilobyte is rounded up to a power of two of pages.
        let memory = Memory::new(2 * region + PAGE);
        let count = (memory.base() + memory.len() - memory.base().next_multiple_of(PAGE)) / PAGE;
        let table = Box::leak(vec![Frame::NEW; count].into_boxed_slice()) as *mut [Frame];
        let mut slabs = Slabs { memory, table, pages: None, caches: Slabs::caches() };
        slabs.reset();
        slabs
    }

    fn caches() -> Vec<Slab> {
        CLASSES.iter().map(|&size| Slab::new("class", size, 16, None, None)).collect()
    }

    fn order(size: usize, align: usize) -> usize {
        size.max(align).div_ceil(PAGE).next_power_of_two().ilog2() as usize
    }

    fn pages(&self) -> &Pages {
        self.pages.as_ref().expect("the frame allocator is there outside `reset`")
    }
}

impl Subject for Slabs {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        let pages = self.pages.as_mut()?;
        match class(size, align) {
            // SAFETY: each cache uses this `Pages` source for its entire lifetime; reset
            // replaces both together before another allocation.
            Some(class) => unsafe { self.caches[class].allocate(pages) },
            None => pages.allocate(Slabs::order(size, align)),
        }
    }

    unsafe fn free(&mut self, addr: usize, size: usize, align: usize) {
        match class(size, align) {
            // SAFETY: the caller's promise, and the same class as when it was allocated.
            Some(class) => unsafe { self.caches[class].free(addr) }.expect("a slab cache refused an object"),
            None => {
                let pages = self.pages.as_mut().expect("the frame allocator is there outside `reset`");
                pages.frames.free(addr).expect("the frame allocator refused a block it had handed out");
            }
        }
    }

    fn used(&self) -> Option<usize> {
        None
    }

    /// The most pages held at once. The frame allocator hands out pages from wherever its
    /// lists have them, as a kernel does with physical memory, and they needn't be next
    /// to each other: the highest address reached says nothing here.
    fn footprint(&self) -> usize {
        self.pages().most * PAGE
    }

    fn searched(&self) -> Option<(usize, usize)> {
        None
    }

    fn check(&self) -> Result<(), &'static str> {
        self.caches.iter().try_for_each(|cache| cache.check())?;
        self.pages().frames.check()
    }

    fn reset(&mut self) {
        self.pages = None; // the old allocator's borrow of the table ends here
        let base = self.memory.base().next_multiple_of(PAGE);
        // SAFETY: the table was leaked in `new`, and nothing else refers to it now. The
        // pages are in `memory`, which the subject keeps.
        let frames = unsafe { Frames::new(base, &mut *self.table, 0) };
        self.pages = Some(Pages { frames, most: 0 });
        self.caches = Slabs::caches();
    }
}

/// One CPU's cache of free blocks in front of TLSF behind its lock, which this one
/// thread always finds free.
struct Caches {
    memory: Memory,
    central: SpinLock<Central<Tlsf>>,
    cache: Cache,
}

impl Caches {
    fn new(region: usize) -> Caches {
        let memory = Memory::new(region);
        let central = Caches::central(&memory);
        Caches { memory, central, cache: Cache::new(BATCH, LIMIT) }
    }

    fn central(memory: &Memory) -> SpinLock<Central<Tlsf>> {
        // SAFETY: the region is the subject's own.
        SpinLock::new(Central::new(unsafe { Tlsf::new(memory.base(), memory.len()) }, BATCH, KEEP))
    }
}

impl Subject for Caches {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        self.cache.allocate(&self.central, size, align)
    }

    unsafe fn free(&mut self, addr: usize, size: usize, align: usize) {
        // SAFETY: the caller's promise.
        unsafe { self.cache.free(&self.central, addr, size, align) }.expect("the cache refused a block");
    }

    /// TLSF's count, which takes a block waiting in the cache or the depot as allocated.
    fn used(&self) -> Option<usize> {
        Some(self.central.lock().heap.used())
    }

    fn footprint(&self) -> usize {
        self.central.lock().heap.footprint()
    }

    fn searched(&self) -> Option<(usize, usize)> {
        None
    }

    fn check(&self) -> Result<(), &'static str> {
        self.central.lock().heap.check()
    }

    fn reset(&mut self) {
        self.central = Caches::central(&self.memory);
        self.cache = Cache::new(BATCH, LIMIT);
    }
}

/// One CPU's heap of pages, after mimalloc, with TLSF for the blocks no class serves.
struct Shards {
    memory: Memory,
    /// From `Box::into_raw`. `heap` borrows it, and a reset frees it after replacing `heap`.
    arena: *mut Arena,
    heap: green_tea_shard::Heap<'static>,
    large: Large,
}

impl Shards {
    fn new(region: usize) -> Shards {
        // The arena keeps its page headers at the start of its region, and its pages are 16 KiB.
        let memory = Memory::new(region + region / 64 + 4 * green_tea_shard::PAGE);
        let arena = Shards::make_arena(&memory);
        // SAFETY: the arena was just made, and lives until `reset` replaces it and this heap.
        let heap = unsafe { &*arena }.heap(RETAIN);
        Shards { memory, arena, heap, large: Large::new(region) }
    }

    fn make_arena(memory: &Memory) -> *mut Arena {
        // SAFETY: the region is the subject's own.
        Box::into_raw(Box::new(unsafe { Arena::new(memory.base(), memory.len()) }))
    }

    fn arena(&self) -> &Arena {
        // SAFETY: `arena` is valid from `new` on: `reset` replaces it with another.
        unsafe { &*self.arena }
    }
}

impl Subject for Shards {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        match class(size, align) {
            Some(_) => self.heap.allocate(size, align),
            None => self.large.tlsf.allocate(size, align),
        }
    }

    unsafe fn free(&mut self, addr: usize, size: usize, align: usize) {
        // SAFETY: the caller's promise, and the same class as when it was allocated.
        unsafe {
            match class(size, align) {
                Some(_) => self.heap.free(addr).expect("the heap of pages refused a block"),
                None => self.large.tlsf.free(addr).expect("TLSF refused a block it had handed out"),
            }
        }
    }

    fn used(&self) -> Option<usize> {
        None
    }

    /// The arena's pages up to the highest one a heap has been given, and what TLSF reached.
    fn footprint(&self) -> usize {
        self.arena().footprint() + self.large.tlsf.footprint()
    }

    fn searched(&self) -> Option<(usize, usize)> {
        None
    }

    fn check(&self) -> Result<(), &'static str> {
        self.heap.check()?;
        self.large.tlsf.check()
    }

    fn reset(&mut self) {
        let old = self.arena;
        self.arena = Shards::make_arena(&self.memory);
        // SAFETY: the new arena lives until the next reset, which replaces this heap too.
        self.heap = unsafe { &*self.arena }.heap(RETAIN);
        // SAFETY: the only heap that borrowed the old arena was replaced on the line above.
        drop(unsafe { Box::from_raw(old) });
        self.large.reset();
    }
}
