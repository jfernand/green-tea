#import "../lib.typ": *

= green-tea-shard <crate-shard>

`green-tea-shard` allocates from several CPUs with no lock, after mimalloc. An `Arena` is a region cut into *pages* of 16 KiB. A page in use belongs to one `Heap`, holds blocks of one size class, and keeps its free blocks on three lists, one of which any CPU may push to with a compare-and-swap. The crate also holds a lock-free `Stack` of page indexes, and a `Queue` after snmalloc's message queue, built to be measured beside it.

#spec(
  ("Source", [#repo("green-tea-shard/src/"), 3 files, 763 lines without their tests]),
  ("Depends on", [`green-tea-fit` (`FreeError`), `green-tea-smp` (the size classes); `no_std`]),
  ("Used by", [`green-tea-coop` (the `shard` kernel), `green-tea-measure`]),
  ("Tests", [15 host tests on real threads, the `shard` kernel]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [crate root], [`Arena`, `Heap`, `Stats`, `PAGE`, and `CLASSES` and `class`, re-exported from @crate-smp],
  [`stack`], [`Stack`, the `Links` trait],
  [`queue`], [`Queue`],
)

== Memory orderings

The crate's atomics, what each operation orders, and why. In each row the synchronizing pair is what makes the *plain* memory behind it safe to read.

#dtable(columns: (auto, auto, 1fr), ("Operation", "Ordering", "What it protects"),
  [`thread_free` CAS, in `free_with`], [`AcqRel`; failure `Acquire`], [Publishes a freed block. The `Release` orders the plain write of the block's link before the push. The `Acquire` makes each pusher see the chain it links to, so the chain's links are ordered through every earlier pusher],
  [`thread_free.load`, before the CAS], [`Acquire`], [The first value of `head`, only stored into the block. A stale value fails the CAS],
  [`thread_free.swap(0)`, in `collect`], [`AcqRel`], [The owner takes the chain. The `Acquire` pairs with the pushers' `Release`, so the owner's plain walk along `next` reads links that are complete],
  [`thread_free.load`, in `collect`], [`Relaxed`], [A hint: is there anything to take. A stale 0 delays a collection and loses nothing, and the read keeps the cache line shared],
  [`block.store`, `owner.store` in `adopt`], [`Release`], [Publishes the page's block size, then its owner. A reader that sees the owner sees the size],
  [`owner`, `block` loads], [`Acquire`], [The other half of that pair, in `free_with`, `size_of` and `check`],
  [`owner.store(0)`, in `give_back`], [`Release`], [Marks the page unowned before it goes on the stack],
  [`Stack::head` CAS], [`AcqRel`; failure `Acquire`], [Hands a page between heaps. A push releases what its owner wrote in the page's `Local` half. The pop that takes it acquires it, so the next owner's plain `Cell` accesses follow the last owner's],
  [`Stack::head`, first load], [`Acquire`], [The start of each loop. A stale head fails the CAS],
  [`link` store, in `Stack::push`], [`Release`], [The entry's link, written before the head that exposes it],
  [`link` load, in `Stack::pop`], [`Acquire`], [May be stale. The tag in the head detects that],
  [`Queue::back.swap`], [`AcqRel`], [The `Release` publishes the sender's cleared last link. The `Acquire` gives the next sender a block whose link it may overwrite],
  [`link(before).store(first)`], [`Release`], [Connects the sender's chain, and publishes its contents, to the owner],
  [`link(oldest).load`, in `take`], [`Acquire`], [The owner's half of that pair],
  [`link(last).store(0)`, `link(first)` in `new`], [`Relaxed`], [A block not yet visible to anyone. The swap that follows publishes it],
  [`heaps`, `owned`, `most`, `reached`], [`Relaxed`], [`heaps` needs only unique ids. The others are statistics],
  [`retries`, `pops`], [`Relaxed`], [Statistics],
)

== Pages and headers

The headers of all pages are together at the start of the arena's region, and each is two cache lines:

#srcfile("green-tea-shard/src/lib.rs", lines: ((48, 84),))[```rust
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
```]

`Local` is what only the owner touches, in `Cell`s. `Shared` is what other CPUs read and write, with a cache line of its own, so that a CPU pushing to `thread_free` doesn't take the line the owner allocates from. `#[repr(align(64))]` on both makes a `Page` 128 bytes.

Each page has three free lists, linked through the blocks' first words:

- `free`, which the owner allocates from;
- `local_free`, where the owner puts the blocks it frees;
- `thread_free`, where every other CPU puts the blocks it frees.

Separating `free` from `local_free` is mimalloc's design. Allocation drains one list while frees build another, so the slow path comes at regular intervals, and that is where the owner does its housekeeping.

The block's page comes from its address, by arithmetic on `PAGE`, which is why `Heap::free` needs no size:

#srcfile("green-tea-shard/src/lib.rs", lines: ((179, 182),))[```rust
fn page_of(&self, addr: usize) -> Option<usize> {
    let index = addr.checked_sub(self.base)? >> SHIFT;
    (self.first..self.pages).contains(&index).then_some(index)
}
```]

== `Arena`

`Arena::new` takes a region, aligns it to pages, reserves the first `first` pages for headers and pushes the others on the stack of unused pages, top first, so the lowest is popped first:

#srcfile("green-tea-shard/src/lib.rs", lines: ((134, 156),))[```rust
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
```]

`Arena::heap(retain)` makes a `Heap` with a new id, for one thread or one CPU. `retain` is how many pages of each class the heap keeps when they empty.

A page changes hands through two private methods, `claim` and `give_back`, which pop and push the stack and keep the counts. The public ones report: `capacity` (pages of blocks), `owned` (pages owned now and at most), `footprint` (bytes up to the highest page ever given out), `stack_counts` (pops and retries), and `size_of`, the size of the block at an address.

== `Heap`

=== Allocation

The fast path touches the first page of the class's list, and nothing another CPU writes:

#srcfile("green-tea-shard/src/lib.rs", lines: ((267, 284),))[```rust
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
```]

When `free` is empty, `generic` collects the class's pages in turn until one has a block, and moves that page to the front. If none has, it claims a new page:

#srcfile("green-tea-shard/src/lib.rs", lines: ((288, 310),))[```rust
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
```]

`collect` takes the whole of `thread_free` with one swap and adds it to `local_free`. If `free` is empty, `local_free` becomes `free`:

#srcfile("green-tea-shard/src/lib.rs", lines: ((322, 342),), notes: ((326, [A `Relaxed` load first: while there is nothing to take, the owner never writes the shared line.]), (327, [One swap takes every block, however many there are. `AcqRel` because the owner then follows their links.]), (335, [`used` counts a block as out until it is collected, so a page with blocks waiting on `thread_free` can't be given back.]),))[```rust
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
```]

`adopt` makes an unused page into a page of a class. It links every block on `free` in address order and publishes the block size and owner. `link` and `unlink` maintain the doubly linked list of a class's pages, in the `prev` and `next` of `Local`, as indexes plus one.

=== Free

`free` is `free_with`, with an empty hook. `free_with` calls `between` after it has read the head and linked the block, which lets a test do at that moment what another CPU might:

#srcfile("green-tea-shard/src/lib.rs", lines: ((407, 447),), notes: ((411, [An unowned page holds nothing allocated.]), (417, [The owner's path: no atomic write.]), (427, [An empty page goes back, unless that leaves fewer than `retain` of the class.]), (434, [The comment is the safety argument. The block is still counted in `used`, so the page can't change owner before the push completes.]), (439, [The push. A failed CAS gives the current head, and the block is relinked to it.]),))[```rust
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
```]

`give_back` unlinks the page and clears `owner` before pushing it on the stack. `collect_all` collects every page and returns the empty ones, down to `retain` of each class: what a thread with nothing else to do might do.

The rest of the interface reports. `pages` is how many the heap owns, `used` how many of its blocks are out, and `free_bytes` the bytes in blocks that aren't. `check` verifies that each block is on one of the three lists or counted as out, and is meaningful only while no other CPU is freeing into the heap's pages. `Stats` counts `allocations`, `local_frees`, `remote_frees`, `retries`, `generic`, `collections`, `collected`, `claimed`, `given_back` and `refused`.

== `stack`: a lock-free stack of indexes

`Stack` holds small numbers, indexes into a table the caller keeps through the `Links` trait. The arena's table is its page headers. The stack itself is one 64-bit word, the *head*:

#bytefield(caption: [The head of a `Stack`],
  ("tag: pops so far", "high 32 bits", 1), ("index on top, plus one", "low 32 bits", 1))

The tag solves the *ABA problem*. A pop reads the head and the top entry's link. Before its compare-and-swap, other CPUs can pop that entry, pop others, and push the first one back. The index on top is what the pop saw, and the link it read is stale. Every pop adds one to the tag, so a head that has been popped from no longer compares equal:

#srcfile("green-tea-shard/src/stack.rs", lines: ((77, 95),), notes: ((86, [The table is never freed, so this read is always safe, and may be stale.]), (87, [`between` is a test hook, as in `free_with`.]), (88, [The new head: the tag plus one, and the link as the new top.]),))[```rust
pub fn pop_with<L: Links + ?Sized>(&self, links: &L, mut between: impl FnMut()) -> Option<usize> {
    let mut head = self.head.load(Ordering::Acquire);
    loop {
        let top = (head & TOP) as usize;
        if top == 0 {
            return None;
        }
        // The entry may have been popped by someone else by now. Its link is in the
        // table, which is never freed, so the read is safe, and may be stale.
        let below = links.link(top - 1).load(Ordering::Acquire);
        between();
        let new = (head & !TOP).wrapping_add(POP) | below as u64;
        match self.head.compare_exchange(head, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(top - 1),
            Err(seen) => head = seen,
        }
        self.retries.fetch_add(1, Ordering::Relaxed);
    }
}
```]

A push doesn't change the tag. It can't invalidate anyone's read: for a head to return to an old value, its entry must first come off, and that is a pop. `push` and `pop` are the operations, `pop_with` is `pop` with a hook, `pops` reads the tag and `retries` counts the CAS failures. The `Links` implementation for `[AtomicU32]` lets the tests use a plain array.

== `queue`: snmalloc's message queue

`Queue` is not part of the allocator. It is the queue snmalloc sends freed blocks home on: many CPUs add, one takes out. It adds a whole chain with one atomic exchange, and nothing is compared or retried:

#srcfile("green-tea-shard/src/queue.rs", lines: ((65, 70),), notes: ((67, [The chain is now in the queue, but can't be reached from the front.]), (68, [A test hook, at the moment that matters.]), (69, [The chain is connected.]),))[```rust
pub unsafe fn add_with(&self, first: usize, last: usize, between: impl FnOnce()) {
    link(last).store(0, Ordering::Relaxed);
    let before = self.back.swap(last, Ordering::AcqRel);
    between();
    link(before).store(first, Ordering::Release);
}
```]

Between a sender's swap and its write, its chain and everything added after it are unreachable from the front. The owner sees them when the write is made. So a stalled sender holds up all the senders behind it. `add` never retries, and `take` can report nothing while blocks are in the queue. `take` uses no atomic exchange:

#srcfile("green-tea-shard/src/queue.rs", lines: ((76, 84),))[```rust
pub unsafe fn take(&self) -> Option<usize> {
    let oldest = self.front.get();
    let after = link(oldest).load(Ordering::Acquire);
    if after == 0 {
        return None;
    }
    self.front.set(after);
    Some(oldest)
}
```]

The block at the front is handed over only once it has a successor, so the queue is never empty: the newest block stays until another arrives, and `Queue::new` takes a first block to start with. `add` is `add_with` with an empty hook. A test, `a_chain_added_behind_an_unfinished_one_waits_for_it`, reproduces the snmalloc paper's figure with that hook.

== How it is exercised

There are 9 tests of the allocator, 3 of the stack and 3 of the queue. Three of them carry the concurrency argument:

- `a_pop_that_was_overtaken_starts_again` is the ABA problem, by hand.
- `eight_threads_freeing_each_others_blocks_lose_none` runs the allocator's concurrent path.
- `four_senders_lose_nothing_and_each_ones_blocks_stay_in_order` runs the queue's.

#output(title: "cargo test -p green-tea-shard")[
```
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `shard` kernel compares a heap of pages per CPU with the caches of @crate-smp on four harts. First, with each CPU freeing its own blocks:

#output(title: "cargo xtask kernel --kernel shard --target riscv64", size: 7.0pt)[
```
each CPU makes 20000 allocations and frees, of 1 to 512 bytes, holding up to 96:
  a cache for each CPU, as in the smp test:  80128 operations, and the lock was taken 2680 times
    the heap behind the lock made 21440 allocations and frees
    when the work ended the caches held 528 blocks, of 105984 bytes in all
  this run: 736 of those waited for another CPU; 310 ms
  a heap of pages for each CPU:              80128 operations, no lock, and 0 frees of another CPU's block
    the CPUs took 56 pages of 16384 bytes from the arena, and gave back 16
    a page's free list ran out 1015 times in 40064 allocations
    when the work ended the CPUs held no blocks, and their heaps held 40 pages, of 655360 bytes in all
  this run: at most 42 pages held at once; 7 pushes and pops of the arena's stack tried again; 270 ms
  the same, with heaps that keep no empty page: the CPUs took 4120 pages and gave back 4120
  this run: at most 41 pages held at once; 176 pushes and pops of the arena's stack tried again; 378 ms
```
]

The lock is gone, and the memory is the cost: 655,360 bytes held at the end, where the caches held 105,984. With `retain` 0, pages move on and off the stack 4,120 times. The second experiment is the case the design is for, every block freed by a CPU that didn't allocate it:

#output(title: "the same run", size: 6.3pt)[
```
each CPU allocates 48 blocks and leaves them for the other three to free, 200 times:
  a cache for each CPU:          38400 allocations and as many frees, and the lock was taken 1992 times
    the heap behind the lock made 15936 allocations and frees
    when the work ended the caches held 384 blocks, of 72448 bytes in all
  this run: 600 of those waited for another CPU; 334 ms
  a heap of pages for each CPU:  38400 allocations and as many frees, no lock, and 38400 frees of another CPU's block
    when every CPU had collected its pages, 38400 blocks had come back to the CPU that allocated them, and 0 were out
    the heaps then held 40 pages, of 655360 bytes in all
  this run: 135 of the compare-and-swaps were tried again; the owners took their pages' lists 832 times
  this run: the CPUs took 40 pages, and held 40 when the rounds ended; 285 ms
```
]

All 38,400 blocks went home through `thread_free`, 135 CAS attempts failed, and the owners took their lists 832 times.

== Limits

- Requests over 1,024 bytes, or aligned to more than 16, are refused. There is no large-object path.
- A `Heap` has no `Drop`. Its pages stay owned when it is gone, and blocks freed into them afterwards wait on a `thread_free` nobody collects.
- The double-free checks are partial: an owner's free fails only when the page has no block out, and a remote free isn't checked at all. Either can corrupt a list.
- The owner's *free* reads `owner` and `block`, which share a line with `thread_free`, so remote pushes do slow local frees. Only allocation is isolated from them.
- `generic` walks a class's pages in turn, so a slow-path allocation costs time in proportion to the pages held.
- The stack's tag is 32 bits. A pop that sleeps across exactly $2^32$ other pops, with the same entry on top, would pass its compare-and-swap with a stale link. `pops` wraps at the same point.
- The stack needs a 64-bit atomic.

One owner per page, and a list for everyone else. @crate-smr deals with freeing what another thread may still be reading.
