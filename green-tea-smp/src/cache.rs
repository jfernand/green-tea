//! Per-CPU caches of free blocks, in front of a heap behind a lock.
//!
//! A [`Cache`] belongs to one thread or one hart. It keeps a list of free blocks for each
//! of twelve size classes, threaded through the blocks themselves as green-tea-pool's free
//! list is. An allocation takes the first block of its class's list, and a free puts the
//! block back at the front. Neither takes a lock, because nobody else can reach the list.
//!
//! The lists are stocked from a [`Central`], which several caches share behind one
//! [`SpinLock`]. A cache whose list is empty takes the lock once and leaves with a
//! *batch* of blocks. A cache whose list has grown past its *limit* takes the lock once
//! and hands a batch back. So the lock is taken once per batch, not once per block.
//!
//! The `Central` is a heap and a *depot*. The depot holds batches that caches handed
//! back, each still chained together, so that the next cache to run dry gets one whole,
//! by moving two pointers. Only when the depot is empty does a refill go to the heap, and
//! only when it is full does a returned batch go back there.
//!
//! To the heap, a block in a cache or in the depot is allocated. The heap's count of used
//! bytes therefore includes every block that is waiting in a cache, on any CPU.
//!
//! A request larger than the largest class, or aligned to more than 16 bytes, goes
//! straight to the heap, under the lock.

use crate::lock::{Relax, SpinLock};
use green_tea_fit::{ALIGN, FreeError, Heap};

/// The sizes the caches keep. A request is served by the smallest that holds it.
pub const CLASSES: [usize; 12] = [16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024];
const N: usize = CLASSES.len();
const WORD: usize = size_of::<usize>();

/// The class that serves a request, or `None` if it goes straight to the heap.
pub fn class(size: usize, align: usize) -> Option<usize> {
    if align > ALIGN {
        return None;
    }
    CLASSES.iter().position(|&c| c >= size)
}

/// A list of free blocks, linked through each block's first word. 0 ends it.
#[derive(Clone, Copy)]
struct List {
    head: usize,
    len: usize,
}

const EMPTY: List = List { head: 0, len: 0 };

fn next(block: usize) -> usize {
    // SAFETY: every block on a list came from the heap, is at least 16 bytes at a multiple
    // of 16, and belongs to nobody but the list.
    unsafe { *(block as *const usize) }
}

fn set_next(block: usize, to: usize) {
    // SAFETY: as for `next`.
    unsafe { *(block as *mut usize) = to }
}

/// What a [`Central`] has been asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CentralStats {
    /// Times a cache came for a batch, and how many of those the depot served.
    pub refills: usize,
    pub from_depot: usize,
    /// Times a cache handed a batch back, and how many of those the depot kept.
    pub flushes: usize,
    pub to_depot: usize,
}

/// What the caches share: the heap, and a depot of batches.
pub struct Central<H> {
    pub heap: H,
    /// For each class, the batches held: a list of lists. A batch's first block has the
    /// next batch's address in its second word.
    depot: [List; N],
    batch: usize,
    keep: usize,
    pub stats: CentralStats,
}

impl<H: Heap> Central<H> {
    /// `batch` is how many blocks a cache takes or returns at a time. `keep` is how many
    /// batches of each class the depot holds before it returns blocks to the heap. With
    /// `keep` 0 there is no depot.
    pub const fn new(heap: H, batch: usize, keep: usize) -> Central<H> {
        assert!(batch >= 1);
        Central {
            heap,
            depot: [EMPTY; N],
            batch,
            keep,
            stats: CentralStats { refills: 0, from_depot: 0, flushes: 0, to_depot: 0 },
        }
    }

    /// A batch of blocks of class `class`, chained: from the depot if it has one, or else
    /// newly allocated from the heap, as many as the heap will give, up to a batch.
    fn take(&mut self, class: usize) -> List {
        self.stats.refills += 1;
        let depot = &mut self.depot[class];
        if depot.len > 0 {
            let head = depot.head;
            (depot.head, depot.len) = (next(head + WORD), depot.len - 1);
            self.stats.from_depot += 1;
            return List { head, len: self.batch };
        }
        let mut list = EMPTY;
        let mut tail = 0;
        while list.len < self.batch {
            let Some(block) = self.heap.allocate(CLASSES[class], ALIGN) else { break };
            set_next(block, 0);
            if tail == 0 {
                list.head = block
            } else {
                set_next(tail, block)
            }
            tail = block;
            list.len += 1;
        }
        list
    }

    /// Take back a chain of blocks of class `class`. A whole batch goes to the depot if
    /// the depot has room. Anything else is freed to the heap, block by block.
    fn give(&mut self, class: usize, list: List) {
        self.stats.flushes += 1;
        let depot = &mut self.depot[class];
        if list.len == self.batch && depot.len < self.keep {
            set_next(list.head + WORD, depot.head);
            (depot.head, depot.len) = (list.head, depot.len + 1);
            self.stats.to_depot += 1;
            return;
        }
        self.free_chain(list.head);
    }

    fn free_chain(&mut self, mut block: usize) {
        while block != 0 {
            let after = next(block);
            // SAFETY: the block came from this heap, through a cache, and is on no list now.
            unsafe { self.heap.free(block) }.expect("the heap refused a block that a cache held");
            block = after;
        }
    }

    /// Free every block in the depot to the heap.
    pub fn drain(&mut self) {
        for class in 0..N {
            while self.depot[class].len > 0 {
                let head = self.depot[class].head;
                (self.depot[class].head, self.depot[class].len) = (next(head + WORD), self.depot[class].len - 1);
                self.free_chain(head);
            }
        }
    }

    /// Bytes in the blocks the depot holds, counting each block as its class's size.
    pub fn depot_bytes(&self) -> usize {
        (0..N).map(|class| self.depot[class].len * self.batch * CLASSES[class]).sum()
    }
}

/// What a [`Cache`] has done.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CacheStats {
    /// Allocations and frees of a size the cache keeps, whether or not they took the lock.
    pub allocations: usize,
    pub frees: usize,
    /// Allocations that found their list empty, and took the lock for a batch.
    pub refills: usize,
    /// Frees that left their list over the limit, and took the lock to hand a batch back.
    pub flushes: usize,
    /// Allocations and frees that went straight to the heap, under the lock.
    pub large: usize,
}

/// One CPU's free blocks.
pub struct Cache {
    lists: [List; N],
    batch: usize,
    limit: usize,
    pub stats: CacheStats,
}

impl Cache {
    /// A cache that moves `batch` blocks at a time, and holds at most `limit` of a class.
    /// A `limit` of `usize::MAX` makes a cache that never gives a block back.
    pub const fn new(batch: usize, limit: usize) -> Cache {
        assert!(batch >= 1 && limit >= batch);
        Cache {
            lists: [EMPTY; N],
            batch,
            limit,
            stats: CacheStats { allocations: 0, frees: 0, refills: 0, flushes: 0, large: 0 },
        }
    }

    /// A block of at least `size` bytes at a multiple of `align`, or `None` if the heap
    /// has nothing left.
    pub fn allocate<H: Heap, R: Relax>(
        &mut self,
        central: &SpinLock<Central<H>, R>,
        size: usize,
        align: usize,
    ) -> Option<usize> {
        let Some(class) = class(size, align) else {
            self.stats.large += 1;
            return central.lock().heap.allocate(size, align);
        };
        self.stats.allocations += 1;
        let list = &mut self.lists[class];
        if list.head == 0 {
            self.stats.refills += 1;
            *list = central.lock().take(class);
        }
        let block = list.head;
        if block == 0 {
            return None;
        }
        (list.head, list.len) = (next(block), list.len - 1);
        Some(block)
    }

    /// Give back a block, with the size and alignment it was asked for.
    ///
    /// # Safety
    /// `addr` must have come from `allocate`, on this cache or on another that shares the
    /// same `central`, with this `size` and `align`, and not have been freed since.
    pub unsafe fn free<H: Heap, R: Relax>(
        &mut self,
        central: &SpinLock<Central<H>, R>,
        addr: usize,
        size: usize,
        align: usize,
    ) -> Result<(), FreeError> {
        let Some(class) = class(size, align) else {
            self.stats.large += 1;
            // SAFETY: the caller's promise.
            return unsafe { central.lock().heap.free(addr) };
        };
        self.stats.frees += 1;
        let list = &mut self.lists[class];
        set_next(addr, list.head);
        (list.head, list.len) = (addr, list.len + 1);
        if list.len > self.limit {
            // Cut the first `batch` blocks off the list, before taking the lock.
            let head = list.head;
            let mut last = head;
            for _ in 1..self.batch {
                last = next(last);
            }
            (list.head, list.len) = (next(last), list.len - self.batch);
            set_next(last, 0);
            self.stats.flushes += 1;
            central.lock().give(class, List { head, len: self.batch });
        }
        Ok(())
    }

    /// Hand every block back, as a thread does when it ends. Takes the lock once.
    pub fn flush_all<H: Heap, R: Relax>(&mut self, central: &SpinLock<Central<H>, R>) {
        let mut central = central.lock();
        for list in &mut self.lists {
            central.free_chain(list.head);
            *list = EMPTY;
        }
    }

    /// How many blocks the cache holds.
    pub fn blocks(&self) -> usize {
        self.lists.iter().map(|list| list.len).sum()
    }

    /// Bytes in the blocks the cache holds, counting each block as its class's size.
    pub fn bytes(&self) -> usize {
        (0..N).map(|class| self.lists[class].len * CLASSES[class]).sum()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{CLASSES, Cache, Central, class};
    use crate::lock::{Relax, SpinLock};
    use green_tea_fit::tlsf::Tlsf;
    use green_tea_fit::trace::Random;
    use std::sync::mpsc;
    use std::thread;
    use std::vec;
    use std::vec::Vec;

    struct Yield;

    impl Relax for Yield {
        fn relax(spins: usize) {
            if spins % 64 == 63 { thread::yield_now() } else { core::hint::spin_loop() }
        }
    }

    type Shared = SpinLock<Central<Tlsf>, Yield>;

    /// A heap over `len` bytes, with caches that move `batch` blocks and a depot that
    /// keeps `keep` batches. The memory is leaked: the tests are short.
    fn shared(len: usize, batch: usize, keep: usize) -> Shared {
        let memory = vec![0u128; len / 16].leak();
        SpinLock::new(Central::new(unsafe { Tlsf::new(memory.as_mut_ptr() as usize, len) }, batch, keep))
    }

    /// A block of class `c` costs the heap its size and two tags.
    fn cost(c: usize) -> usize {
        CLASSES[c] + 16
    }

    #[test]
    fn a_request_is_served_by_the_smallest_class_that_holds_it() {
        assert_eq!(
            (class(1, 1), class(16, 8), class(17, 16), class(100, 8), class(1024, 16)),
            (Some(0), Some(0), Some(1), Some(5), Some(11))
        );
        assert_eq!((class(1025, 8), class(16, 32)), (None, None));
    }

    #[test]
    fn the_lock_is_taken_once_for_a_batch_of_allocations() {
        let central = shared(1 << 16, 8, 0);
        let mut cache = Cache::new(8, 16);
        let first = cache.allocate(&central, 40, 8).unwrap();
        assert_eq!(central.counts().0, 1);
        assert_eq!(central.lock().heap.used(), 8 * cost(2)); // the heap has given out a batch
        assert_eq!(cache.blocks(), 7);
        let rest: Vec<usize> = (0..7).map(|_| cache.allocate(&central, 40, 8).unwrap()).collect();
        assert_eq!(central.counts().0, 2); // one more, for the `used` above
        assert!(rest.iter().all(|&b| b != first));
        cache.allocate(&central, 40, 8).unwrap();
        assert_eq!(central.counts().0, 3);
        assert_eq!((cache.stats.allocations, cache.stats.refills), (9, 2));
    }

    #[test]
    fn a_free_goes_to_the_front_of_the_list_and_is_allocated_next() {
        let central = shared(1 << 16, 8, 0);
        let mut cache = Cache::new(8, 16);
        let a = cache.allocate(&central, 16, 8).unwrap();
        let _b = cache.allocate(&central, 16, 8).unwrap();
        unsafe { cache.free(&central, a, 16, 8) }.unwrap();
        assert_eq!(cache.allocate(&central, 16, 8), Some(a));
        assert_eq!(central.counts().0, 1);
    }

    #[test]
    fn a_cache_over_its_limit_hands_a_batch_back_to_the_heap() {
        let central = shared(1 << 16, 4, 0);
        let mut cache = Cache::new(4, 8);
        let blocks: Vec<usize> = (0..12).map(|_| cache.allocate(&central, 64, 8).unwrap()).collect();
        assert_eq!((cache.blocks(), central.lock().heap.used()), (0, 12 * cost(3)));
        for &block in &blocks[..8] {
            unsafe { cache.free(&central, block, 64, 8) }.unwrap();
        }
        assert_eq!((cache.blocks(), cache.stats.flushes), (8, 0)); // at the limit, not over it
        unsafe { cache.free(&central, blocks[8], 64, 8) }.unwrap();
        assert_eq!((cache.blocks(), cache.stats.flushes), (5, 1));
        assert_eq!(central.lock().heap.used(), 8 * cost(3)); // 3 live, 5 cached
    }

    #[test]
    fn a_depot_passes_a_batch_from_one_cache_to_another_without_the_heap() {
        let central = shared(1 << 16, 4, 2);
        let (mut one, mut two) = (Cache::new(4, 4), Cache::new(4, 4));
        let blocks: Vec<usize> = (0..8).map(|_| one.allocate(&central, 64, 8).unwrap()).collect();
        let before = central.lock().heap.stats();
        // The second cache frees what the first allocated: at the fifth free it is over its limit.
        for &block in &blocks[..5] {
            unsafe { two.free(&central, block, 64, 8) }.unwrap();
        }
        assert_eq!((two.blocks(), central.lock().depot_bytes()), (1, 4 * 64));
        // The first cache is empty, and its refill is the batch the second gave up.
        let again = one.allocate(&central, 64, 8).unwrap();
        assert!(blocks[..5].contains(&again));
        let mut central = central.lock();
        assert_eq!(central.heap.stats(), before); // the heap saw none of it
        assert_eq!((central.stats.from_depot, central.stats.to_depot), (1, 1));
        central.drain();
        assert_eq!(central.depot_bytes(), 0);
    }

    #[test]
    fn a_depot_that_is_full_sends_the_batch_to_the_heap() {
        let central = shared(1 << 16, 2, 1);
        let mut cache = Cache::new(2, 2);
        let blocks: Vec<usize> = (0..8).map(|_| cache.allocate(&central, 16, 8).unwrap()).collect();
        for &block in &blocks {
            unsafe { cache.free(&central, block, 16, 8) }.unwrap();
        }
        // 8 frees with a limit of 2: flushes at the 3rd, 5th and 7th. The depot keeps one.
        let central = central.lock();
        assert_eq!((central.stats.flushes, central.stats.to_depot), (3, 1));
        assert_eq!(central.heap.used(), 4 * cost(0)); // 2 in the depot, 2 in the cache
    }

    #[test]
    fn large_and_aligned_requests_go_straight_to_the_heap() {
        let central = shared(1 << 16, 8, 0);
        let mut cache = Cache::new(8, 16);
        let large = cache.allocate(&central, 2000, 8).unwrap();
        let aligned = cache.allocate(&central, 64, 256).unwrap();
        assert!(aligned.is_multiple_of(256));
        assert_eq!((cache.blocks(), cache.stats.large, central.counts().0), (0, 2, 2));
        unsafe { cache.free(&central, large, 2000, 8) }.unwrap();
        unsafe { cache.free(&central, aligned, 64, 256) }.unwrap();
        assert_eq!(central.lock().heap.used(), 0);
    }

    #[test]
    fn an_exhausted_heap_is_an_allocation_refused() {
        let central = shared(256, 8, 0);
        let mut cache = Cache::new(8, 16);
        let got: Vec<usize> = (0..8).map_while(|_| cache.allocate(&central, 64, 8)).collect();
        assert!(!got.is_empty() && got.len() < 8);
        assert_eq!(cache.allocate(&central, 64, 8), None);
    }

    /// Each thread allocates and frees at random through a cache of its own, writing its
    /// number over every block it holds and checking it before the free.
    fn churn(central: &Shared, thread: usize, operations: usize) -> Cache {
        let mut cache = Cache::new(8, 16);
        let mut random = Random(thread as u64 + 1);
        let mut held: Vec<(usize, usize)> = Vec::new();
        for _ in 0..operations {
            if held.len() < 64 && (held.is_empty() || random.below(2) == 0) {
                let size = 1 + random.below(1500);
                let addr = cache.allocate(central, size, 8).expect("the region is large enough");
                unsafe { core::ptr::write_bytes(addr as *mut u8, thread as u8, size) };
                held.push((addr, size));
            } else {
                let (addr, size) = held.swap_remove(random.below(held.len()));
                let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                assert!(bytes.iter().all(|&b| b == thread as u8), "a block was handed to two threads");
                unsafe { cache.free(central, addr, size, 8) }.unwrap();
            }
        }
        for (addr, size) in held {
            unsafe { cache.free(central, addr, size, 8) }.unwrap();
        }
        cache
    }

    #[test]
    fn four_threads_with_a_cache_each_leave_the_heap_whole() {
        for keep in [0, 4] {
            let central = shared(4 << 20, 8, keep);
            let caches: Vec<Cache> = thread::scope(|s| {
                let threads: Vec<_> = (0..4)
                    .map(|t| {
                        s.spawn({
                            let central = &central;
                            move || churn(central, t, 50_000)
                        })
                    })
                    .collect();
                threads.into_iter().map(|t| t.join().unwrap()).collect()
            });
            // Everything the program allocated is freed. What the heap still counts as used
            // is in the caches and the depot.
            let cached: usize = caches.iter().map(|c| c.blocks() * 16 + c.bytes()).sum();
            let (used, in_depot) = central.with(|c| (c.heap.used(), c.depot_bytes()));
            assert!(used >= cached + in_depot && used > 0);
            for mut cache in caches {
                cache.flush_all(&central);
            }
            let mut central = central.lock();
            central.drain();
            assert_eq!(central.heap.used(), 0);
            central.heap.check().unwrap();
        }
    }

    #[test]
    fn blocks_freed_by_another_thread_come_back_through_the_central_heap() {
        let central = shared(1 << 21, 8, 0);
        let (send, receive) = mpsc::channel::<usize>();
        let (produced, consumed) = thread::scope(|s| {
            let producer = s.spawn(|| {
                let mut cache = Cache::new(8, 16);
                for _ in 0..10_000 {
                    send.send(cache.allocate(&central, 100, 8).unwrap()).unwrap();
                }
                drop(send);
                cache
            });
            let consumer = s.spawn(|| {
                let mut cache = Cache::new(8, 16);
                for addr in receive {
                    unsafe { cache.free(&central, addr, 100, 8) }.unwrap();
                }
                cache
            });
            (producer.join().unwrap(), consumer.join().unwrap())
        });
        // The producer never frees, so every batch is a refill; the consumer never
        // allocates, so its list only grows, and it flushes a batch for every 8 frees.
        assert_eq!((produced.stats.refills, produced.stats.flushes), (10_000 / 8, 0));
        assert_eq!((consumed.stats.refills, consumed.stats.flushes), (0, (10_000 - 16) / 8));
        assert_eq!(consumed.blocks(), 16);
        let (mut produced, mut consumed) = (produced, consumed);
        produced.flush_all(&central);
        consumed.flush_all(&central);
        assert_eq!(central.lock().heap.used(), 0);
    }
}
