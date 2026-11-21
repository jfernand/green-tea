//! Allocating from several CPUs with no lock, after mimalloc.
//!
//! An [`Arena`] is a region of memory cut into *pages* of 16 KiB. (These are the
//! allocator's pages, four of the processor's.) A page in use belongs to one [`Heap`] and
//! holds blocks of one size class. Each CPU, or each thread, has a `Heap` of its own.
//!
//! The free blocks of a page are on lists that live in that page's header, and there are
//! three of them:
//!
//! - `free`, which the owner allocates from;
//! - `local_free`, where the owner puts the blocks it frees;
//! - `thread_free`, where every other CPU puts the blocks it frees, with a
//!   compare-and-swap.
//!
//! An allocation pops the first block of `free`, and touches nothing that another CPU
//! writes. When `free` is empty the owner *collects* the page: it takes the whole of
//! `thread_free` with one atomic swap, and `local_free` becomes `free`. So a block freed
//! by another CPU goes back to the page it came from, and is handed out again by the heap
//! that owns the page.
//!
//! [`Heap::free`] takes an address and nothing else. The page a block is in is found from
//! its address, and the page's header gives the block's size and the heap that owns it.
//!
//! Pages that no heap is using are on a lock-free [`stack::Stack`] in the arena.
//!
//! Requests over 1024 bytes, or aligned to more than 16, are refused.
//!
//! [`queue::Queue`] is not part of this allocator. It is the queue that snmalloc sends
//! freed blocks home on, a batch at a time, built to be measured beside `thread_free`.

#![no_std]

pub mod queue;
pub mod stack;

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use green_tea_fit::FreeError;
pub use green_tea_smp::cache::{CLASSES, class};
use stack::{Links, Stack};

/// The size of a page, and the shift that divides by it.
pub const PAGE: usize = 1 << SHIFT;
const SHIFT: usize = 14;
const N: usize = CLASSES.len();

/// What only the page's owner reads and writes.
#[repr(align(64))]
struct Local {
    /// The blocks the owner allocates from, linked through their first words. 0 ends it.
    free: Cell<usize>,
    /// The blocks the owner has freed since it last collected the page.
    local_free: Cell<usize>,
    /// Blocks handed out and not yet known to be back: those the program holds, and those
    /// waiting on `thread_free`.
    used: Cell<u32>,
    /// How many blocks the page is cut into.
    capacity: Cell<u32>,
    class: Cell<u32>,
    /// The pages before and after this one on the owner's list for the class, each as an
    /// index plus one. 0 is none.
    prev: Cell<u32>,
    next: Cell<u32>,
}

/// What other CPUs read and write. It has a cache line of its own, so that a CPU pushing
/// to `thread_free` doesn't take the line the owner allocates from.
#[repr(align(64))]
struct Shared {
    /// The blocks other CPUs have freed since the owner last collected the page.
    thread_free: AtomicUsize,
    /// The id of the heap that owns the page, or 0 if the page is on the arena's stack.
    owner: AtomicUsize,
    /// The size of the page's blocks.
    block: AtomicUsize,
    /// The page's link on the arena's stack of unused pages.
    link: AtomicU32,
}

/// A page's header. The headers are together, at the start of the arena's region.
struct Page {
    local: Local,
    shared: Shared,
}

fn next(block: usize) -> usize {
    // SAFETY: a block on a list is at least 16 bytes at a multiple of 16, inside the arena.
    unsafe { *(block as *const usize) }
}

fn set_next(block: usize, to: usize) {
    // SAFETY: as for `next`.
    unsafe { *(block as *mut usize) = to }
}

/// A region of memory, in pages, shared by the heaps made from it.
pub struct Arena {
    /// The address of page 0. The first `first` pages hold the headers.
    base: usize,
    pages: usize,
    first: usize,
    /// The pages no heap owns.
    unused: Stack,
    heaps: AtomicUsize,
    /// How many pages heaps own now, and the most they ever did.
    owned: AtomicUsize,
    most: AtomicUsize,
    /// One more than the highest page ever given to a heap.
    reached: AtomicUsize,
}

// SAFETY: the arena's own fields are atomics. A page header is written through a shared
// reference: its `Shared` half only atomically, and its `Local` half only by the heap whose
// id is in `owner`, which is one thread's.
unsafe impl Sync for Arena {}
unsafe impl Send for Arena {}

impl Links for Arena {
    fn link(&self, index: usize) -> &AtomicU32 {
        &self.page(index).shared.link
    }
}

impl Arena {
    /// An arena over the whole pages between `start` and `start + len`.
    ///
    /// # Safety
    /// The memory must be the caller's to give, and must outlive the arena and every
    /// block allocated from it.
    ///
    /// # Panics
    /// If the region doesn't hold two pages.
    pub unsafe fn new(start: usize, len: usize) -> Arena {
        let base = start.next_multiple_of(PAGE);
        let pages = (start + len).saturating_sub(base) >> SHIFT;
        let first = (pages * size_of::<Page>()).div_ceil(PAGE);
        assert!(pages > first, "an arena needs room for its headers and a page");
        let arena = Arena {
            base,
            pages,
            first,
            unused: Stack::new(),
            heaps: AtomicUsize::new(0),
            owned: AtomicUsize::new(0),
            most: AtomicUsize::new(0),
            reached: AtomicUsize::new(first),
        };
        for index in 0..pages {
            // SAFETY: the headers are inside the region, which the caller gave us.
            unsafe { (base as *mut Page).add(index).write_bytes(0, 1) };
        }
        // Pushed from the top down, so that the lowest page is the first to be popped.
        for index in (first..pages).rev() {
            arena.unused.push(&arena, index);
        }
        arena
    }

    /// A heap that allocates from this arena, for one thread or one CPU. When a page of
    /// the heap's has no block in use, the heap gives the page back, unless that would
    /// leave it with fewer than `retain` pages of that class.
    pub fn heap(&self, retain: usize) -> Heap<'_> {
        let id = self.heaps.fetch_add(1, Ordering::Relaxed) + 1;
        Heap { arena: self, id, lists: [0; N], held: [0; N], retain, stats: Stats::default() }
    }

    fn page(&self, index: usize) -> &Page {
        debug_assert!(index < self.pages);
        // SAFETY: `new` zeroed `pages` headers at `base`, and zero is a valid header.
        unsafe { &*(self.base as *const Page).add(index) }
    }

    /// The address of page `index`'s first block.
    fn start(&self, index: usize) -> usize {
        self.base + (index << SHIFT)
    }

    /// The page that `addr` is in, if it is in one that holds blocks.
    fn page_of(&self, addr: usize) -> Option<usize> {
        let index = addr.checked_sub(self.base)? >> SHIFT;
        (self.first..self.pages).contains(&index).then_some(index)
    }

    /// The size of the block at `addr`, as the page's header has it, if `addr` is the
    /// start of a block in a page that a heap owns.
    pub fn size_of(&self, addr: usize) -> Option<usize> {
        let index = self.page_of(addr)?;
        let shared = &self.page(index).shared;
        let size = shared.block.load(Ordering::Acquire);
        (shared.owner.load(Ordering::Acquire) != 0 && (addr - self.start(index)).is_multiple_of(size)).then_some(size)
    }

    /// Take a page nobody owns.
    fn claim(&self) -> Option<usize> {
        let index = self.unused.pop(self)?;
        let owned = self.owned.fetch_add(1, Ordering::Relaxed) + 1;
        self.most.fetch_max(owned, Ordering::Relaxed);
        self.reached.fetch_max(index + 1, Ordering::Relaxed);
        Some(index)
    }

    fn give_back(&self, index: usize) {
        self.owned.fetch_sub(1, Ordering::Relaxed);
        self.unused.push(self, index);
    }

    /// How many pages of blocks the arena has.
    pub fn capacity(&self) -> usize {
        self.pages - self.first
    }

    /// How many pages heaps own now, and the most they have owned at once.
    pub fn owned(&self) -> (usize, usize) {
        (self.owned.load(Ordering::Relaxed), self.most.load(Ordering::Relaxed))
    }

    /// The bytes from the arena's first page of blocks to the end of the highest page
    /// any heap has been given.
    pub fn footprint(&self) -> usize {
        (self.reached.load(Ordering::Relaxed) - self.first) << SHIFT
    }

    /// How many times a page was taken from the stack of unused pages, and how many
    /// times a push or a pop there had to start again.
    pub fn stack_counts(&self) -> (usize, usize) {
        (self.unused.pops(), self.unused.retries())
    }
}

/// What a [`Heap`] has done.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub allocations: usize,
    /// Frees of blocks in the heap's own pages.
    pub local_frees: usize,
    /// Frees of blocks in another heap's pages: each a compare-and-swap on that page.
    pub remote_frees: usize,
    /// Times a remote free's compare-and-swap failed, and was tried again.
    pub retries: usize,
    /// Allocations that found their page's `free` list empty.
    pub generic: usize,
    /// Times a page's `thread_free` list was taken, and how many blocks were on it.
    pub collections: usize,
    pub collected: usize,
    /// Pages taken from the arena, and pages given back.
    pub claimed: usize,
    pub given_back: usize,
    /// Requests no class serves.
    pub refused: usize,
}

/// One CPU's pages, by size class.
pub struct Heap<'a> {
    arena: &'a Arena,
    id: usize,
    /// For each class, the first page of the heap's list, as an index plus one.
    lists: [u32; N],
    /// For each class, how many pages are on the list.
    held: [u32; N],
    retain: usize,
    pub stats: Stats,
}

impl Heap<'_> {
    /// A block of at least `size` bytes at a multiple of `align`. `None` if the arena has
    /// no page left, or if no class serves the request.
    pub fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        let Some(class) = class(size, align) else {
            self.stats.refused += 1;
            return None;
        };
        self.stats.allocations += 1;
        let first = self.lists[class] as usize;
        if first != 0 {
            let local = &self.arena.page(first - 1).local;
            let block = local.free.get();
            if block != 0 {
                local.free.set(next(block));
                local.used.set(local.used.get() + 1);
                return Some(block);
            }
        }
        self.generic(class)
    }

    /// The slow path: the first page of the class has nothing on `free`. Collect the
    /// class's pages in turn until one has a block, or else take a new page.
    fn generic(&mut self, class: usize) -> Option<usize> {
        self.stats.generic += 1;
        let mut at = self.lists[class] as usize;
        while at != 0 {
            let index = at - 1;
            self.collect(index);
            let local = &self.arena.page(index).local;
            if local.free.get() != 0 {
                // The next allocation starts here.
                self.unlink(class, index);
                self.link(class, index);
                return Some(self.pop(index));
            }
            at = local.next.get() as usize;
        }
        let Some(index) = self.arena.claim() else {
            self.stats.allocations -= 1;
            return None;
        };
        self.stats.claimed += 1;
        self.adopt(class, index);
        Some(self.pop(index))
    }

    fn pop(&mut self, index: usize) -> usize {
        let local = &self.arena.page(index).local;
        let block = local.free.get();
        local.free.set(next(block));
        local.used.set(local.used.get() + 1);
        block
    }

    /// Bring home what was freed in page `index`: everything other CPUs have pushed on
    /// `thread_free`, and what this heap put on `local_free`.
    fn collect(&mut self, index: usize) {
        let page = self.arena.page(index);
        // The load is a read of a line the owner may still have. The swap is a write, and
        // is made only when there is something to take.
        if page.shared.thread_free.load(Ordering::Relaxed) != 0 {
            let chain = page.shared.thread_free.swap(0, Ordering::AcqRel);
            let (mut last, mut blocks) = (chain, 1u32);
            while next(last) != 0 {
                last = next(last);
                blocks += 1;
            }
            set_next(last, page.local.local_free.get());
            page.local.local_free.set(chain);
            page.local.used.set(page.local.used.get() - blocks);
            self.stats.collections += 1;
            self.stats.collected += blocks as usize;
        }
        if page.local.free.get() == 0 {
            page.local.free.set(page.local.local_free.replace(0));
        }
    }

    /// Make an unused page into a page of `class`, with every block on `free`, in order
    /// of address, and put it first on the class's list.
    fn adopt(&mut self, class: usize, index: usize) {
        let page = self.arena.page(index);
        let (size, start) = (CLASSES[class], self.arena.start(index));
        let capacity = PAGE / size;
        for block in 0..capacity {
            let at = start + block * size;
            set_next(at, if block + 1 < capacity { at + size } else { 0 });
        }
        page.local.free.set(start);
        page.local.local_free.set(0);
        page.local.used.set(0);
        page.local.capacity.set(capacity as u32);
        page.local.class.set(class as u32);
        page.shared.block.store(size, Ordering::Release);
        page.shared.owner.store(self.id, Ordering::Release);
        self.link(class, index);
    }

    /// Put page `index` first on the list of `class`.
    fn link(&mut self, class: usize, index: usize) {
        let local = &self.arena.page(index).local;
        let first = self.lists[class];
        local.prev.set(0);
        local.next.set(first);
        if first != 0 {
            self.arena.page(first as usize - 1).local.prev.set(index as u32 + 1);
        }
        self.lists[class] = index as u32 + 1;
        self.held[class] += 1;
    }

    fn unlink(&mut self, class: usize, index: usize) {
        let local = &self.arena.page(index).local;
        let (prev, next) = (local.prev.get(), local.next.get());
        match prev {
            0 => self.lists[class] = next,
            _ => self.arena.page(prev as usize - 1).local.next.set(next),
        }
        if next != 0 {
            self.arena.page(next as usize - 1).local.prev.set(prev);
        }
        self.held[class] -= 1;
    }

    /// Give back the block at `addr`.
    ///
    /// # Safety
    /// `addr` must have come from `allocate`, on this heap or on another of the same
    /// arena, and not have been freed since.
    pub unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError> {
        // SAFETY: the caller's promise.
        unsafe { self.free_with(addr, || ()) }
    }

    /// [`free`](Heap::free), calling `between` each time a free to another heap's page
    /// has read that page's `thread_free` and linked the block to it, and before it tries
    /// to replace the list's head. The tests use it to do, at that moment, what another
    /// CPU might.
    ///
    /// # Safety
    /// As for [`free`](Heap::free).
    pub unsafe fn free_with(&mut self, addr: usize, mut between: impl FnMut()) -> Result<(), FreeError> {
        let index = self.arena.page_of(addr).ok_or(FreeError::NotABlock)?;
        let page = self.arena.page(index);
        let owner = page.shared.owner.load(Ordering::Acquire);
        if owner == 0 {
            return Err(FreeError::NotAllocated); // the page is not in use, so nothing in it is
        }
        if !(addr - self.arena.start(index)).is_multiple_of(page.shared.block.load(Ordering::Acquire)) {
            return Err(FreeError::NotABlock);
        }
        if owner == self.id {
            let local = &page.local;
            if local.used.get() == 0 {
                return Err(FreeError::NotAllocated);
            }
            set_next(addr, local.local_free.get());
            local.local_free.set(addr);
            local.used.set(local.used.get() - 1);
            self.stats.local_frees += 1;
            let class = local.class.get() as usize;
            if local.used.get() == 0 && self.held[class] as usize > self.retain {
                self.give_back(class, index);
            }
            return Ok(());
        }
        // Another heap's page. While this block is out, the page can't change hands, so
        // the owner read above stays true until the push below has happened.
        let list = &page.shared.thread_free;
        let mut head = list.load(Ordering::Acquire);
        loop {
            set_next(addr, head);
            between();
            match list.compare_exchange(head, addr, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(seen) => head = seen,
            }
            self.stats.retries += 1;
        }
        self.stats.remote_frees += 1;
        Ok(())
    }

    /// Return a page with no block in use to the arena.
    fn give_back(&mut self, class: usize, index: usize) {
        let page = self.arena.page(index);
        debug_assert!(page.local.used.get() == 0 && page.shared.thread_free.load(Ordering::Relaxed) == 0);
        self.unlink(class, index);
        page.shared.owner.store(0, Ordering::Release);
        self.arena.give_back(index);
        self.stats.given_back += 1;
    }

    /// Collect every page the heap owns, and give back those with no block in use, down
    /// to `retain` of each class. What a thread with nothing else to do might do.
    pub fn collect_all(&mut self) {
        for class in 0..N {
            let mut at = self.lists[class] as usize;
            while at != 0 {
                let index = at - 1;
                self.collect(index);
                let local = &self.arena.page(index).local;
                at = local.next.get() as usize;
                if local.used.get() == 0 && self.held[class] as usize > self.retain {
                    self.give_back(class, index);
                }
            }
        }
    }

    /// How many pages the heap owns.
    pub fn pages(&self) -> usize {
        self.held.iter().map(|&pages| pages as usize).sum()
    }

    /// How many of the heap's blocks are out: held by the program, or freed by another
    /// CPU and not yet collected.
    pub fn used(&self) -> usize {
        let mut blocks = 0;
        self.each_page(|_, local| blocks += local.used.get() as usize);
        blocks
    }

    /// Bytes in the blocks of the heap's pages that are not out.
    pub fn free_bytes(&self) -> usize {
        let mut bytes = 0;
        self.each_page(|_, local| {
            bytes += (local.capacity.get() - local.used.get()) as usize * CLASSES[local.class.get() as usize]
        });
        bytes
    }

    fn each_page(&self, mut f: impl FnMut(usize, &Local)) {
        for class in 0..N {
            let mut at = self.lists[class] as usize;
            while at != 0 {
                let local = &self.arena.page(at - 1).local;
                f(at - 1, local);
                at = local.next.get() as usize;
            }
        }
    }

    /// Walk every page the heap owns and check that each block is on exactly one of the
    /// page's three lists or is counted as out. Returns how many blocks are out. Only
    /// meaningful while no other CPU is freeing into this heap's pages.
    pub fn check(&self) -> Result<usize, &'static str> {
        let mut out = 0;
        for class in 0..N {
            let (mut at, mut pages, mut before) = (self.lists[class] as usize, 0, 0);
            while at != 0 {
                let index = at - 1;
                let page = self.arena.page(index);
                if page.shared.owner.load(Ordering::Acquire) != self.id {
                    return Err("a page on a heap's list is owned by another");
                }
                if page.local.class.get() as usize != class || page.local.prev.get() as usize != before {
                    return Err("a page is on the wrong list, or its links disagree");
                }
                let (size, start) = (CLASSES[class], self.arena.start(index));
                let capacity = page.local.capacity.get() as usize;
                let mut seen = [0u64; PAGE / 16 / 64];
                let mut listed = 0;
                let heads = [
                    page.local.free.get(),
                    page.local.local_free.get(),
                    page.shared.thread_free.load(Ordering::Acquire),
                ];
                for head in heads {
                    let mut block = head;
                    while block != 0 {
                        let offset = block.wrapping_sub(start);
                        if offset >= PAGE || !offset.is_multiple_of(size) {
                            return Err("a free list leads out of its page, or into the middle of a block");
                        }
                        let number = offset / size;
                        if seen[number / 64] & (1 << (number % 64)) != 0 {
                            return Err("a block is on a free list twice");
                        }
                        seen[number / 64] |= 1 << (number % 64);
                        listed += 1;
                        block = next(block);
                    }
                }
                // Blocks on `thread_free` are still counted in `used`.
                let mut waiting = 0;
                let mut block = heads[2];
                while block != 0 {
                    waiting += 1;
                    block = next(block);
                }
                if listed - waiting + page.local.used.get() as usize != capacity {
                    return Err("a page's count of blocks in use disagrees with its free lists");
                }
                out += page.local.used.get() as usize - waiting;
                (before, at, pages) = (at, page.local.next.get() as usize, pages + 1);
            }
            if pages != self.held[class] as usize {
                return Err("a heap's count of its pages disagrees with its list");
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Arena, CLASSES, FreeError, PAGE};
    use green_tea_fit::trace::Random;
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::vec;
    use std::vec::Vec;

    /// An arena with `pages` pages of blocks. The memory is leaked: the tests are short.
    fn arena(pages: usize) -> Arena {
        // Room for the headers as well, and a page that is lost to alignment.
        let headers = (1..).find(|&h| ((pages + h) * size_of::<super::Page>()).div_ceil(PAGE) == h).unwrap();
        let len = (pages + headers + 1) * PAGE;
        let memory = vec![0u128; len / 16].leak();
        let arena = unsafe { Arena::new(memory.as_mut_ptr() as usize + 16, len - 16) };
        assert_eq!(arena.capacity(), pages);
        arena
    }

    #[test]
    fn a_page_hands_out_its_blocks_in_order_of_address() {
        let arena = arena(4);
        let mut heap = arena.heap(1);
        let blocks: Vec<usize> = (0..3).map(|_| heap.allocate(100, 8).unwrap()).collect();
        assert_eq!((blocks[1] - blocks[0], blocks[2] - blocks[1]), (128, 128));
        assert_eq!(blocks[0] % PAGE, 0);
        // The size is the page's to know: nothing is kept in or beside the block.
        assert_eq!(arena.size_of(blocks[1]), Some(128));
        assert_eq!(arena.size_of(blocks[1] + 8), None);
        // Another class is another page.
        let other = heap.allocate(40, 8).unwrap();
        assert_eq!((other % PAGE, arena.size_of(other)), (0, Some(48)));
        assert_ne!(other / PAGE, blocks[0] / PAGE);
        assert_eq!((heap.pages(), heap.stats.claimed, arena.footprint()), (2, 2, 2 * PAGE));
        assert_eq!(heap.check(), Ok(4));
    }

    #[test]
    fn a_block_the_owner_frees_waits_until_the_free_list_runs_out() {
        let arena = arena(4);
        let mut heap = arena.heap(1);
        let capacity = PAGE / 128;
        let first = heap.allocate(128, 8).unwrap();
        let second = heap.allocate(128, 8).unwrap();
        unsafe { heap.free(first) }.unwrap();
        // A cache would hand `first` straight back. The page goes on in order.
        assert_eq!(heap.allocate(128, 8), Some(second + 128));
        for _ in 3..capacity {
            heap.allocate(128, 8).unwrap();
        }
        assert_eq!((heap.stats.generic, heap.stats.claimed), (1, 1));
        // `free` is now empty, and the next allocation takes the slow path, which finds `first`.
        assert_eq!(heap.allocate(128, 8), Some(first));
        assert_eq!((heap.stats.generic, heap.stats.claimed, heap.pages()), (2, 1, 1));
        assert_eq!(heap.check(), Ok(capacity));
    }

    #[test]
    fn a_block_freed_by_another_heap_goes_back_to_its_page() {
        let arena = arena(4);
        let (mut ours, mut theirs) = (arena.heap(1), arena.heap(1));
        let block = ours.allocate(64, 8).unwrap();
        unsafe { theirs.free(block) }.unwrap();
        assert_eq!((theirs.stats.remote_frees, theirs.stats.local_frees, theirs.pages()), (1, 0, 0));
        // Until we collect, the block still counts as out.
        assert_eq!(ours.used(), 1);
        // What the other heap allocates next is in a page of its own.
        let other = theirs.allocate(64, 8).unwrap();
        assert_ne!(other / PAGE, block / PAGE);
        // We get the block back when the page's free list runs out.
        let rest: Vec<usize> = (1..PAGE / 64).map(|_| ours.allocate(64, 8).unwrap()).collect();
        assert!(!rest.contains(&block));
        assert_eq!(ours.allocate(64, 8), Some(block));
        assert_eq!((ours.stats.collections, ours.stats.collected, ours.stats.claimed), (1, 1, 1));
        assert_eq!((ours.check(), theirs.check()), (Ok(PAGE / 64), Ok(1)));
    }

    /// Two frees into the same page from two other heaps, interleaved by hand: the second
    /// happens after the first has read the list's head and before it swaps.
    #[test]
    fn a_remote_free_that_was_overtaken_tries_again() {
        let arena = arena(4);
        let (mut owner, mut one, mut two) = (arena.heap(1), arena.heap(1), arena.heap(1));
        let (a, b) = (owner.allocate(64, 8).unwrap(), owner.allocate(64, 8).unwrap());
        let mut overtaken = false;
        unsafe {
            one.free_with(a, || {
                if !overtaken {
                    overtaken = true;
                    two.free(b).unwrap();
                }
            })
        }
        .unwrap();
        assert_eq!((one.stats.retries, two.stats.retries), (1, 0));
        owner.collect_all();
        assert_eq!((owner.stats.collected, owner.used()), (2, 0), "a block freed by another heap was lost");
        assert_eq!(owner.check(), Ok(0));
    }

    #[test]
    fn a_page_with_nothing_in_use_is_given_back() {
        let arena = arena(4);
        let (mut keeps, mut gives) = (arena.heap(1), arena.heap(0));
        let kept = keeps.allocate(64, 8).unwrap();
        unsafe { keeps.free(kept) }.unwrap();
        assert_eq!((keeps.pages(), keeps.stats.given_back), (1, 0));
        let given = gives.allocate(64, 8).unwrap();
        unsafe { gives.free(given) }.unwrap();
        assert_eq!((gives.pages(), gives.stats.given_back, arena.owned()), (0, 1, (1, 2)));
        assert_eq!(unsafe { gives.free(given) }, Err(FreeError::NotAllocated));
        // The page is anyone's now, for any class.
        let again = keeps.allocate(1000, 8).unwrap();
        assert_eq!((again, arena.size_of(again), arena.footprint()), (given, Some(1024), 2 * PAGE));
        // A heap that keeps one page gives back the second.
        let blocks: Vec<usize> = (0..PAGE / 1024 + 1).map(|_| keeps.allocate(1000, 8).unwrap()).collect();
        assert_eq!(keeps.pages(), 3);
        for block in blocks {
            unsafe { keeps.free(block) }.unwrap();
        }
        unsafe { keeps.free(again) }.unwrap();
        assert_eq!((keeps.pages(), keeps.stats.given_back, keeps.check()), (2, 1, Ok(0)));
    }

    #[test]
    fn what_is_not_a_block_is_refused() {
        let arena = arena(2);
        let mut heap = arena.heap(1);
        let block = heap.allocate(64, 8).unwrap();
        assert_eq!(unsafe { heap.free(block + 8) }, Err(FreeError::NotABlock));
        assert_eq!(unsafe { heap.free(block - PAGE) }, Err(FreeError::NotABlock)); // a page of headers
        assert_eq!(unsafe { heap.free(block + PAGE) }, Err(FreeError::NotAllocated)); // a page nobody owns
        assert_eq!(unsafe { heap.free(16) }, Err(FreeError::NotABlock));
        assert_eq!((heap.allocate(1025, 8), heap.allocate(64, 32), heap.stats.refused), (None, None, 2));
        unsafe { heap.free(block) }.unwrap();
        assert_eq!(unsafe { heap.free(block) }, Err(FreeError::NotAllocated));
    }

    #[test]
    fn an_arena_with_no_page_left_refuses_and_recovers() {
        let arena = arena(2);
        let mut heap = arena.heap(0);
        let blocks: Vec<usize> = (0..2 * (PAGE / 1024)).map(|_| heap.allocate(1024, 8).unwrap()).collect();
        assert_eq!((heap.allocate(1024, 8), heap.allocate(16, 8)), (None, None));
        assert_eq!(heap.stats.allocations, blocks.len());
        for &block in &blocks[..PAGE / 1024] {
            unsafe { heap.free(block) }.unwrap();
        }
        assert!(heap.allocate(16, 8).is_some());
        assert_eq!(heap.check(), Ok(PAGE / 1024 + 1));
    }

    /// The producer and consumer of Chapter 9, in strict turns: one thread allocates 64
    /// blocks, another frees them. The producer needs one page, however long it goes on.
    #[test]
    fn a_producer_and_a_consumer_need_one_page() {
        const ROUNDS: usize = 200;
        let arena = arena(8);
        let (send, receive) = mpsc::channel::<Vec<usize>>();
        let (reply, answer) = mpsc::channel::<()>();
        let (produced, consumed) = thread::scope(|s| {
            let arena = &arena;
            let producer = s.spawn(move || {
                let mut heap = arena.heap(1);
                for _ in 0..ROUNDS {
                    send.send((0..64).map(|_| heap.allocate(100, 8).unwrap()).collect()).unwrap();
                    answer.recv().unwrap();
                }
                heap.collect_all();
                (heap.stats, heap.check())
            });
            let consumer = s.spawn(move || {
                let mut heap = arena.heap(1);
                for blocks in receive {
                    for block in blocks {
                        unsafe { heap.free(block) }.unwrap();
                    }
                    reply.send(()).unwrap();
                }
                heap.stats
            });
            (producer.join().unwrap(), consumer.join().unwrap())
        });
        assert_eq!((arena.footprint(), arena.owned()), (PAGE, (1, 1)));
        assert_eq!((consumed.remote_frees, consumed.retries, consumed.claimed), (ROUNDS * 64, 0, 0));
        // A page of 128 blocks lasts two rounds: one collection for every two.
        assert_eq!((produced.0.collected, produced.0.collections), (ROUNDS * 64, ROUNDS / 2));
        assert_eq!(produced.1, Ok(0));
    }

    /// Eight threads allocate, fill each block with their own number, and free about half
    /// of what they allocate themselves. The rest they send to a thread chosen at random,
    /// which checks the block's bytes and frees it. At the end nothing is in use.
    #[test]
    fn eight_threads_freeing_each_others_blocks_lose_none() {
        const THREADS: usize = 8;
        const OPERATIONS: usize = 100_000;
        let arena = arena(1024);
        let channels: Vec<_> = (0..THREADS).map(|_| mpsc::sync_channel::<(usize, usize, u8)>(256)).collect();
        let (senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
        let (begin, sent, drained) = (Barrier::new(THREADS), Barrier::new(THREADS), Barrier::new(THREADS));
        let results: Vec<_> = thread::scope(|s| {
            let spawned: Vec<_> =
                receivers
                    .into_iter()
                    .enumerate()
                    .map(|(me, inbox)| {
                        let (arena, senders, begin, sent, drained) = (&arena, senders.clone(), &begin, &sent, &drained);
                        s.spawn(move || {
                            let mut heap = arena.heap(1);
                            let mut random = Random(me as u64 + 1);
                            let mut kept: Vec<(usize, usize, u8)> = Vec::new();
                            let mut wrong = 0;
                            let mut release = |heap: &mut super::Heap, (addr, size, tag): (usize, usize, u8)| {
                                let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                                wrong += bytes.iter().filter(|&&byte| byte != tag).count();
                                unsafe { heap.free(addr) }.unwrap();
                            };
                            begin.wait();
                            for _ in 0..OPERATIONS {
                                let size = 1 + random.below(512);
                                let addr = heap.allocate(size, 8).expect("the arena is large enough");
                                unsafe { core::ptr::write_bytes(addr as *mut u8, me as u8 + 1, size) };
                                match random.below(4) {
                                    // To another thread, or to ourselves, unless its queue is full.
                                    0 | 1 => {
                                        if let Err(full) =
                                            senders[random.below(THREADS)].try_send((addr, size, me as u8 + 1))
                                        {
                                            let (mpsc::TrySendError::Full(block)
                                            | mpsc::TrySendError::Disconnected(block)) = full;
                                            release(&mut heap, block);
                                        }
                                    }
                                    2 => kept.push((addr, size, me as u8 + 1)),
                                    _ => release(&mut heap, (addr, size, me as u8 + 1)),
                                }
                                if kept.len() > 64 {
                                    let which = random.below(kept.len());
                                    release(&mut heap, kept.swap_remove(which));
                                }
                                while let Ok(block) = inbox.try_recv() {
                                    release(&mut heap, block);
                                }
                            }
                            for block in kept.drain(..) {
                                release(&mut heap, block);
                            }
                            sent.wait(); // nobody sends after this
                            while let Ok(block) = inbox.try_recv() {
                                release(&mut heap, block);
                            }
                            drained.wait(); // nobody frees after this
                            heap.collect_all();
                            (wrong, heap.check(), heap.stats)
                        })
                    })
                    .collect();
            spawned.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let mut remote = 0;
        for (wrong, check, stats) in results {
            assert_eq!(wrong, 0, "a block's bytes changed while a thread held it: it was handed out twice");
            assert_eq!(check, Ok(0), "blocks were lost");
            assert_eq!(stats.allocations, stats.local_frees + stats.collected);
            remote += stats.remote_frees;
        }
        assert!(remote > THREADS * OPERATIONS / 10, "the threads should have freed each other's blocks");
        assert!(arena.owned().0 <= THREADS * CLASSES.len());
    }
}
