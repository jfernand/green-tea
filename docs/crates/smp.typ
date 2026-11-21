#import "../lib.typ": *

= green-tea-smp <crate-smp>

`green-tea-smp` makes an allocator usable from several CPUs at once. `SpinLock` is mutual exclusion with one atomic flag. `Cache` is one CPU's own stock of free blocks, by size class, which it allocates from and frees to without a lock. `Central` is what the caches share: the heap and a depot of spare batches, behind one `SpinLock`.

#spec(
  ("Source", [#repo("green-tea-smp/src/"), 3 files, 434 lines without their tests]),
  ("Depends on", [`green-tea-fit`; `no_std`]),
  ("Used by", [`green-tea-bare`, `green-tea-shard`, `green-tea-measure`]),
  ("Tests", [13 host tests on real threads, a doc test that must not compile, the `smp` kernel]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [`lock`], [`SpinLock`, its `Guard`, the `Relax` trait, `Spin`],
  [`cache`], [`Cache`, `CacheStats`, `Central`, `CentralStats`, `CLASSES`, `class`],
)

Neither module knows what a thread is. The caller gives each thread, or each hart, a `Cache` of its own, and hands it the same `SpinLock<Central<_>>`.

== Memory orderings

Every atomic operation in the crate is in `lock.rs`. The cache has none: what it shares, it shares through the lock.

#dtable(columns: (auto, auto, 1fr), ("Operation", "Ordering", "What it protects"),
  [`held.swap(true)`, both places], [`Acquire`], [Taking the lock. Everything the previous holder wrote before its release, the `T` and the blocks it links, is visible to the new holder],
  [`held.store(false)` in `Guard::drop`], [`Release`], [Giving it up. All writes made under the lock are ordered before the store, and so before the next holder's `Acquire`],
  [`held.load` while waiting], [`Relaxed`], [Nothing. The loop only decides when to try the swap again, and the swap is what synchronizes],
  [`contended.fetch_add`], [`Relaxed`], [A statistic. It orders nothing, and needs only to lose no increment],
  [`acquisitions` load and store], [`Relaxed`], [A statistic, written only by the holder, so a separate load and store lose nothing. The lock's own ordering makes each holder see the last value],
  [`counts()`], [`Relaxed`], [Two independent reads for reporting. They are not a consistent pair while CPUs are working],
)

A block's contents move between CPUs by the same route. A cache that frees a block writes the link into it, and hands the chain over inside `central.lock()`. The `Release` when that guard drops and the `Acquire` of the CPU that next takes the lock are what make the links readable to it. A block on a cache's own list is seen by one CPU only and needs no ordering.

== `SpinLock`

#srcfile("green-tea-smp/src/lock.rs", lines: ((30, 38), (41, 41), (56, 60),), notes: ((59, [A guard hands out the `T` as a `&mut T` would, so it is `Sync` only if `T` is.]),))[```rust
pub struct SpinLock<T, R = Spin> {
    held: AtomicBool,
    /// Times the lock was taken. Written only by whoever holds the lock.
    acquisitions: AtomicUsize,
    /// Times a CPU found the lock held and had to wait.
    contended: AtomicUsize,
    value: UnsafeCell<T>,
    relax: PhantomData<fn() -> R>,
}
// …
unsafe impl<T: Send, R> Sync for SpinLock<T, R> {}
// …
pub struct Guard<'a, T, R = Spin> {
    lock: &'a SpinLock<T, R>,
    /// The guard gives access to a `T` as a `&mut T` would, and is `Sync` only if that is.
    value: PhantomData<&'a mut T>,
}
```]

The lock is *test and test-and-set*. The first attempt is a swap. A CPU that loses waits by reading, which leaves the flag's cache line shared between the waiting CPUs, and swaps again only when it sees the flag clear:

#srcfile("green-tea-smp/src/lock.rs", lines: ((75, 96),), notes: ((78, [The uncontended path: one atomic swap.]), (84, [`Relaxed` loads while waiting. A swap here would take the cache line exclusively on every iteration.]), (88, [Another waiter may win between the load and this swap, so it is in a loop.]), (94, [Not a `fetch_add`: the lock is held, so nobody else writes this count.]),))[```rust
pub fn lock(&self) -> Guard<'_, T, R> {
    // The swap writes `true` and returns what was there, in one step that no other
    // CPU can come between. If it returns `false`, the flag was clear and is now ours.
    if self.held.swap(true, Ordering::Acquire) {
        self.contended.fetch_add(1, Ordering::Relaxed);
        let mut spins = 0;
        loop {
            // Wait with loads, which leave the flag's cache line shared between the
            // waiting CPUs. A swap would take the line for itself each time.
            while self.held.load(Ordering::Relaxed) {
                R::relax(spins);
                spins += 1;
            }
            if !self.held.swap(true, Ordering::Acquire) {
                break;
            }
        }
    }
    // We hold the lock, so nobody else is writing this count.
    self.acquisitions.store(self.acquisitions.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    Guard { lock: self, value: PhantomData }
}
```]

Between two looks, a waiter calls `R::relax(spins)`, from the `Relax` trait. `Spin`, the default, is `core::hint::spin_loop`, right for a kernel where the holder is running on another hart. The tests supply a `Relax` that calls `thread::yield_now` every 64 spins, because their threads may outnumber the host's free CPUs and a thread spinning on a descheduled holder wastes a time slice.

`lock` returns a `Guard`, whose `Deref` and `DerefMut` reach the value and whose `Drop` releases the lock. The `PhantomData` keeps a `Guard<Cell<u32>>` from being `Sync`, because two threads sharing it could write the `Cell` at once, and a `compile_fail` doc test on `Guard` holds it to that. `SpinLock::new` is `const`. `with` runs a closure under the lock, `get_mut` reaches the value through `&mut self` with no locking at all, and `counts` returns the acquisitions and how many of them had to wait.

== `Cache` and `Central`

=== Size classes

`CLASSES` has twelve sizes from 16 to 1,024 bytes, and `class` picks the smallest that holds a request:

#srcfile("green-tea-smp/src/cache.rs", lines: ((28, 28), (33, 38),))[```rust
pub const CLASSES: [usize; 12] = [16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024];
// …
pub fn class(size: usize, align: usize) -> Option<usize> {
    if align > ALIGN {
        return None;
    }
    CLASSES.iter().position(|&c| c >= size)
}
```]

A request larger than 1,024 bytes, or aligned to more than `ALIGN` (16), goes straight to the heap under the lock. Internal fragmentation is bounded by the spacing of the classes: a request of 129 bytes takes a block of 192.

=== The fast path

A `Cache` has a list of free blocks for each class, threaded through the blocks' first words, as in @crate-pool. An allocation pops the first:

#srcfile("green-tea-smp/src/cache.rs", lines: ((201, 223),), notes: ((215, [The lock is taken once, for a batch. The guard is a temporary and is dropped at the end of this statement.]), (219, [The heap had nothing, and `take` returned an empty list.]),))[```rust
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
```]

A free pushes the block at the front, so the block freed last, whose cache lines are the warmest, is the next one handed out. When a list grows past its *limit*, the first `batch` blocks are cut off *before* the lock is taken, and handed over in one acquisition:

#srcfile("green-tea-smp/src/cache.rs", lines: ((243, 258),))[```rust
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
```]

So the lock is taken once per batch and not once per block. `flush_all` hands every block back in one acquisition, as a thread does when it ends. `blocks` and `bytes` report what the cache holds, and `CacheStats` counts `allocations`, `frees`, `refills`, `flushes` and `large`.

=== The depot

`Central` holds the heap and, for each class, a list of whole batches that caches handed back, in Bonwick's magazine-and-depot arrangement. A batch stays chained. Its first block has the next batch's address in its *second* word, because the first word is the chain inside the batch, so the smallest class, 16 bytes on a 64-bit machine, is exactly large enough:

#srcfile("green-tea-smp/src/cache.rs", lines: ((99, 136),), notes: ((104, [A whole batch moves by changing two words: the depot's head and length.]), (111, [Only when the depot is empty does a refill go to the heap, and it may return fewer blocks than a batch.]), (129, [A partial chain, or a full depot, sends the blocks to the heap one at a time.]),))[```rust
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
```]

`Central::new` takes `batch` and `keep`, the batches of each class the depot holds, where `keep` 0 means no depot. `drain` frees every depot block to the heap, `depot_bytes` reports what it holds, and `CentralStats` counts `refills`, `from_depot`, `flushes` and `to_depot`.

To the heap, a block in a cache or in the depot is allocated. The heap's count of used bytes therefore includes every block waiting anywhere.

== How it is exercised

The 13 tests are 3 for the lock and 10 for the caches. Three of them run on several threads:

- `four_threads_adding_under_the_lock_lose_nothing`
- `four_threads_with_a_cache_each_leave_the_heap_whole`
- `blocks_freed_by_another_thread_come_back_through_the_central_heap`

#output(title: "cargo test -p green-tea-smp")[
```
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `smp` kernel runs on four harts. The first experiment is the lock. Each CPU adds 1 to a counter a million times:

#output(title: "cargo xtask kernel --kernel smp --target riscv64")[
```
4 CPUs are running: the one that booted, and 3 it started
each CPU adds 1 to a counter 1000000 times:
  with a lock: the counter is 4000000, and the lock was taken 4000000 times
  this run: without a lock the counter is 1842202; with one, 3965903 acquisitions waited
```
]

Without the lock, 2,157,798 of the 4,000,000 increments were lost in this run. The second experiment makes 20,000 allocations and frees per CPU, three ways:

#output(title: "the same run", size: 5.8pt)[
```
each CPU makes 20000 allocations and frees, of 1 to 512 bytes, holding up to 96:
  the kernel's heap, behind its lock:  80128 operations, and the lock was taken 80128 times
  this run: 63049 of those waited for another CPU; 542 ms
  a cache for each CPU:                80128 operations, and the lock was taken 2680 times
    1373 times to fill a cache with 8 blocks, and 1307 times to take 8 back
    when the work ended the CPUs held no blocks, and their caches held 528, of 105984 bytes in all
  this run: 774 of those waited for another CPU; 317 ms
  the same, with a depot of 4 batches:  80128 operations, and the lock was taken 2680 times
  this run: 152 of those waited; 284 ms; the depot served 1185 of the 1373 fills, and the heap made 2376 allocations and frees
```
]

The acquisitions are the same on every run. The waits and milliseconds are marked "this run" because they aren't. Caches cut the acquisitions by a factor of 30, and hold 105,984 bytes for it when the work ends.

== Limits

- A spin lock isn't fair: nothing orders the waiters, and one can lose repeatedly.
- Taking the lock twice from one CPU waits forever. The `SpinLock` doesn't disable interrupts either, so an interrupt handler that takes a lock its CPU already holds deadlocks.
- A `Cache` has no `Drop`: one dropped without `flush_all` loses its blocks to the heap, which counts them as allocated for good.
- A `Cache` doesn't know which `Central` filled it. The `Cache` and `Central` also take their `batch` separately, and a cache whose batch differs from its central's never gets a batch into the depot.
- A block freed by another CPU goes to the freeing CPU's cache, so a producer and a consumer move every block through the central lock. @crate-shard is built around that case.

One flag, a list per class, and a lock taken 30 times less often. @crate-shard takes no lock at all.
