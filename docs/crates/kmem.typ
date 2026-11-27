#import "../lib.typ": *

= green-tea-kmem <crate-kmem>

`green-tea-kmem` is what a kernel allocates for itself with, in two layers. `frames` hands out page frames in blocks of 1 to 1,024 pages, with a binary buddy system whose records are kept apart from the pages. `slab` cuts those pages into caches of objects of one type, and keeps the objects constructed between uses, after Jeff Bonwick's slab allocator.

#spec(
  ("Source", [#repo("allocators/kmem/src/"), 3 files, 878 lines without their tests]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `kmem` and `task` modules), `green-tea-measure`]),
  ("Tests", [24 host tests, a `compile_fail` doc test, the `slab` kernel]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [crate root], [`PAGE` (4,096), the `PageSource` trait],
  [`frames`], [`Frames`, `Frame`, `FreeError`, `Stats`, `ORDERS`, `MAX_ORDER`],
  [`slab`], [`Cache`, `BoundCache`, `Constructor`, `Destructor`, `FreeError`, `Stats`, `HEADER`],
)

== The `PageSource` trait

The two layers meet in one trait. A slab cache doesn't know where its pages come from:

#srcfile("allocators/kmem/src/lib.rs", lines: ((22, 22), (29, 29), (31, 31), (33, 33), (38, 39),), notes: ((29, [`unsafe`: whoever implements it promises what the trait's safety comment says.]),),)[```rust
/// Where a slab cache gets its pages, and gives them back.
// …
pub unsafe trait PageSource {
// …
    fn take_page(&mut self) -> Option<usize>;
// …
    /// Give back a page that `take_page` returned.
// …
    unsafe fn give_page(&mut self, page: usize);
}
```]

The trait is `unsafe` to implement. Its safety comment is the contract a slab cache relies on: every page is nonzero, page-aligned, writable, at least a page long, and the source's alone until it is given back.

`Frames` implements it with blocks of order 0, and its `give_page` panics if the page wasn't an allocated block of one page. The tests of `slab` use a source of their own, over host memory.

== `frames`: the buddy system

A *block* is $2^"order"$ pages, for an order from 0 to `MAX_ORDER`, which is 10, so there are `ORDERS` = 11 sizes and the largest block is 4 MiB. A block starts at a frame number that is a multiple of its size. Its *buddy* is the other half of the block it was split from, and its frame number differs in one bit.

=== The table of frames

The allocator keeps nothing in the pages. Each page has a `Frame`, in a table the caller provides, and the free lists are threaded through the table by frame number:

#srcfile("allocators/kmem/src/frames.rs", lines: ((31, 38), (43, 50),), notes: ((32, [`Tail` marks every frame of a block but the first. `free` of an address inside a block finds a `Tail` and refuses it.]), (48, [Frame numbers, so a record is 12 bytes on every architecture, and a table can number up to $2^32 - 2$ frames.]),))[```rust
enum State {
    /// Not the first frame of a block.
    Tail,
    /// The first frame of a free block, which is on the list of its order.
    Free,
    /// The first frame of an allocated block.
    Used,
}
// …
pub struct Frame {
    state: State,
    /// For the first frame of a block, the block's order.
    order: u8,
    /// For the first frame of a free block, the next and the previous block on its list.
    next: u32,
    prev: u32,
}
```]

#bytefield(caption: [A `Frame`: 12 bytes, `repr(C)`],
  ("state", "1", 1), ("order", "1", 1), ("padding", "2", 1), ("next", "4", 2), ("prev", "4", 2))

Keeping the records apart has three consequences. The allocator never reads or writes a free page. The order of an allocated block is on record, so `free` takes an address and nothing else. And a free of a block that isn't allocated can be refused, with `FreeError::NotAFrame` for an address that isn't the start of a frame, `FreeError::Reserved` for one of the reserved pages, and `FreeError::NotAllocated` for a free block or the inside of one. The price is the table: the `slab` kernel's 4,096 frames need 12 pages for it.

`Frames::new` takes the table and a count of `reserved` pages at the start of the region, which stay allocated for good. It fills the table with `Frame::NEW`, an allocated block of order 0, and frees the others one at a time, so that the ordinary merging builds the initial free lists:

#srcfile("allocators/kmem/src/frames.rs", lines: ((110, 136),), notes: ((113, [`Frames` is a `PageSource`, which promises pages at nonzero addresses.]), (116, [`PhysicalRange` (@crate-memory) refuses a region whose end would wrap past the top of the address space.]),), size: 6.3pt,)[```rust
pub unsafe fn new(base: usize, table: &'a mut [Frame], reserved: usize) -> Frames<'a> {
    assert!(base.is_multiple_of(PAGE), "frame 0 must start on a page");
    assert!(table.len() < NONE as usize && reserved <= table.len(), "a table too large");
    assert!(table.is_empty() || base != 0, "frame 0 must have a nonzero address");
    let bytes = table.len().checked_mul(PAGE).expect("frame range overflows");
    if bytes != 0 {
        PhysicalRange::new(base, bytes, PAGE).expect("frame range overflows");
    }
    table.fill(Frame::NEW);
    let pages = table.len();
    let mut frames = Frames {
        base,
        table,
        heads: [NONE; ORDERS],
        free: [0; ORDERS],
        reserved,
        used: pages,
        stats: Stats::default(),
    };
    // Every page starts as an allocated block of order 0. Freeing the ones that aren't
    // reserved, one at a time, merges them into the largest blocks that fit.
    for frame in reserved..pages {
        frames.release(frame);
    }
    frames.stats = Stats::default();
    frames
}
```]

=== Allocation and free

`allocate` takes an order. It finds the smallest order with a free block, unlinks that block, and halves it until it is the right size. The upper half of each halving becomes a free block:

#srcfile("allocators/kmem/src/frames.rs", lines: ((141, 160),), notes: ((143, [A search of at most 11 list heads. The cost of an allocation depends on the orders, and never on how many blocks are free.]), (153, [`frame + (1 << half)` is the upper half. The lower half keeps the frame number and is halved again.]),))[```rust
pub fn allocate(&mut self, order: usize) -> Option<usize> {
    // The smallest free block that is large enough.
    let found = (order..ORDERS).find(|at| self.heads[*at] != NONE);
    let Some(from) = found else {
        self.stats.refused += 1;
        return None;
    };
    let frame = self.heads[from] as usize;
    self.unlink(frame, from);
    // Halve it until it is the size asked for. The upper half of each halving is a
    // free block of its own.
    for half in (order..from).rev() {
        self.push(frame + (1 << half), half);
        self.stats.splits += 1;
    }
    self.table[frame] = Frame { state: State::Used, order: order as u8, next: NONE, prev: NONE };
    self.used += 1 << order;
    self.stats.allocations += 1;
    Some(self.base + frame * PAGE)
}
```]

`free` validates the address and calls `release`, which merges for as long as the buddy is free and of the same order:

#srcfile("allocators/kmem/src/frames.rs", lines: ((181, 204),), notes: ((187, [The buddy's frame number: flip the bit of the block's size.]), (192, [A buddy can be `Free` and smaller, when it has been split and its first part is free. The order tells the two apart.]), (197, [The upper of the two becomes a `Tail`, inside the merged block.]),))[```rust
fn release(&mut self, frame: usize) -> usize {
    let freed = self.table[frame].order as usize;
    self.used -= 1 << freed;
    let (mut frame, mut order) = (frame, freed);
    while order < MAX_ORDER {
        // The other half of the block this one was split from.
        let buddy = frame ^ (1 << order);
        if buddy + (1 << order) > self.table.len() {
            break; // the region ends before the buddy does
        }
        let there = self.table[buddy];
        if there.state != State::Free || there.order as usize != order {
            break; // allocated, or split into smaller blocks of which the first is free
        }
        self.unlink(buddy, order);
        // The merged block starts at the lower of the two, and the other is inside it.
        self.table[frame.max(buddy)].state = State::Tail;
        frame = frame.min(buddy);
        order += 1;
        self.stats.merges += 1;
    }
    self.push(frame, order);
    freed
}
```]

The lists are doubly linked because a merge removes the buddy from the middle of its list, in constant time. `push` and `unlink` are the two list operations.

=== What it reports

#dtable(columns: (auto, 1fr), ("Method", "Returns"),
  [`base`, `pages`], [the address of frame 0, and how many pages are managed, the reserved ones included],
  [`range`], [the region as a `PhysicalRange` (@crate-memory), or `None` if it has no pages],
  [`used_pages`, `free_pages`], [pages in allocated blocks (reserved included), and in free blocks],
  [`free_blocks`], [how many free blocks there are of each order],
  [`largest_free`], [the order of the largest free block],
  [`order_of`], [the order of the allocated block that starts at an address],
  [`stats`], [`Stats`: `allocations`, `refused`, `frees`, `splits`, `merges`],
  [`check`], [walks the table and lists, and verifies the invariants],
)

`check` confirms that blocks tile the region, each starts on a multiple of its size, every free block is on the list of its order with correct back links, the counts match, and no two free buddies of one order are left unmerged.

== `slab`: caches of constructed objects

A `Cache` allocates objects of one size. Its *slab* is one page, cut into *buffers* from the start, with the slab's record in the last `HEADER` bytes, 40 on a 64-bit machine. A buffer is the object and, after it, a word that links the slab's free buffers, so the link never overwrites the object:

#bytefield(caption: [A slab of the `slab` kernel's 200-byte objects: 19 buffers of 208 bytes],
  ("colour", "0–104", 1), ("object", "200", 2), ("link", "8", 1), ("… 18 more buffers", "3,744", 4), ("spare", "104 − colour", 1), ("record", "40", 1))

#srcfile("allocators/kmem/src/slab.rs", lines: ((27, 37), (44, 44), (46, 46),))[```rust
struct Slab {
    /// The next and the previous slab on the same list of the cache, or 0.
    next: usize,
    prev: usize,
    /// The first free buffer, or 0.
    free: usize,
    /// How many of the slab's objects are allocated.
    in_use: usize,
    /// The first buffer: the page's address plus the slab's colour.
    first: usize,
}
// …
pub type Constructor = fn(*mut u8) -> bool;
// …
pub type Destructor = fn(*mut u8);
```]

A `Constructor` brings an object into its constructed state, given its address, and may refuse by returning `false`. A `Destructor` undoes it. Bonwick's observation is that initialising an object often costs more than allocating it, and that a freed object can stay initialised. So `allocate` hands out an object as the constructor left it, and `free` expects it back in that state.

=== Making a cache

`Cache::new` is a `const fn`, so a cache can be a `static`. It computes the layout from the size and alignment, and in a `static` a layout that doesn't fit is a build error:

#srcfile("allocators/kmem/src/slab.rs", lines: ((112, 118),), notes: ((114, [The link follows the object, at the next word boundary.]), (118, [The leftover bytes of a page give the number of *colours*: distinct starting offsets, one alignment step apart.]),), size: 6.3pt)[```rust
assert!(align.is_power_of_two() && size > 0, "an object needs a size, and an alignment that is a power of two");
let align = if align < WORD { WORD } else { align };
let link_at = size.next_multiple_of(WORD);
let buf = (link_at + WORD).next_multiple_of(align);
let per_slab = (PAGE - HEADER) / buf;
assert!(per_slab > 0, "an object too large for a slab of one page");
let colours = (PAGE - HEADER - per_slab * buf) / align + 1;
```]

=== Allocation, free and reap

Each cache has three lists of slabs: full, partial and empty, by how many of a slab's objects are allocated. `allocate` prefers a partial slab, then an empty one, and only then makes a new one with `grow`. It is `unsafe`, as are `free` and `reap`, because the cache can't check that every call passes the same page source, and a slab given back to a different source would be freed into the wrong allocator:

#srcfile("allocators/kmem/src/slab.rs", lines: ((153, 181),),)[```rust
pub unsafe fn allocate(&mut self, pages: &mut dyn PageSource) -> Option<usize> {
    // A slab that is partly used first, then one kept from before, then a new one.
    let slab = if self.heads[PARTIAL] != 0 {
        self.heads[PARTIAL]
    } else if self.heads[EMPTY] != 0 {
        self.heads[EMPTY]
    } else {
        match self.grow(pages) {
            Some(slab) => slab,
            None => {
                self.stats.refused += 1;
                return None;
            }
        }
    };
    let header = slab as *mut Slab;
    // SAFETY: `slab` is on one of the cache's lists, so it is a slab's record, and its
    // first free buffer's link is the cache's to read.
    let (object, was) = unsafe {
        let object = (*header).free;
        (*header).free = ((object + self.link_at) as *const usize).read();
        (*header).in_use += 1;
        (object, (*header).in_use - 1)
    };
    self.moved(slab, was, was + 1);
    self.in_use += 1;
    self.stats.allocations += 1;
    Some(object)
}
```]

Filling partial slabs first is what lets the others empty, and an empty slab is what `reap` can return. `grow` takes a page, constructs every object in it, threads the free list in address order and writes the record:

#srcfile("allocators/kmem/src/slab.rs", lines: ((253, 287),), notes: ((264, [A constructor that fails: the ones already built are destroyed, and the page goes back.]), (283, [The next slab starts one colour further on, so objects at one index of successive slabs don't share an offset in the page, and compete less for the same lines of the processor's cache.]),),)[```rust
fn grow(&mut self, pages: &mut dyn PageSource) -> Option<usize> {
    let page = pages.take_page()?;
    let first = page + self.next_colour * self.align;
    let object = |index: usize| first + index * self.buf;
    if let Some(constructor) = self.constructor {
        for index in 0..self.per_slab {
            if constructor(object(index) as *mut u8) {
                continue;
            }
            // Undo the ones that were constructed, and do without the slab.
            if let Some(destructor) = self.destructor {
                (0..index).for_each(|built| destructor(object(built) as *mut u8));
                self.stats.destroyed += index;
            }
            self.stats.constructed += index;
            // SAFETY: the page came from `pages` a moment ago.
            unsafe { pages.give_page(page) };
            return None;
        }
        self.stats.constructed += self.per_slab;
    }
    // The free list, in address order: each buffer's link is the next buffer.
    for index in 0..self.per_slab {
        let next = if index + 1 < self.per_slab { object(index + 1) } else { 0 };
        // SAFETY: the page is ours, and the link is inside its buffer.
        unsafe { ((object(index) + self.link_at) as *mut usize).write(next) };
    }
    let slab = page + PAGE - HEADER;
    // SAFETY: the last `HEADER` bytes of the page, which no buffer reaches.
    unsafe { (slab as *mut Slab).write(Slab { next: 0, prev: 0, free: first, in_use: 0, first }) };
    self.next_colour = (self.next_colour + 1) % self.colours;
    self.push(slab, EMPTY);
    self.stats.slabs_created += 1;
    Some(slab)
}
```]

`free` finds the slab from the address alone, by masking to the page. It refuses an address that isn't where an object starts (`NotAnObject`), and walks the slab's free list, at most `per_slab` steps, to refuse a second free (`NotAllocated`):

#srcfile("allocators/kmem/src/slab.rs", lines: ((193, 215),))[```rust
let slab = (object & !(PAGE - 1)) + PAGE - HEADER;
let header = slab as *mut Slab;
// SAFETY: by the caller's promise the page is one of the cache's slabs.
unsafe {
    let offset = object.checked_sub((*header).first).ok_or(FreeError::NotAnObject)?;
    if !offset.is_multiple_of(self.buf) || offset / self.buf >= self.per_slab {
        return Err(FreeError::NotAnObject);
    }
    // The slab's free list is all there is to say which objects are free, so a
    // second free is found by walking it: at most `per_slab` steps.
    let mut free = (*header).free;
    while free != 0 {
        if free == object {
            return Err(FreeError::NotAllocated);
        }
        free = ((free + self.link_at) as *const usize).read();
    }
    ((object + self.link_at) as *mut usize).write((*header).free);
    (*header).free = object;
    (*header).in_use -= 1;
    let now = (*header).in_use;
    self.moved(slab, now + 1, now);
}
```]

A slab that empties stays in the cache, constructed. `Cache::reap` is the only way a page goes back: it runs the destructor on every object of each empty slab and calls `give_page`.

=== A cache bound to its source

`Cache::bind` removes the `unsafe` from those three calls. It moves the cache into a `BoundCache` that holds the page source by exclusive reference, so every call goes to the one source, and the source can't be dropped or used elsewhere while the cache lives:

#srcfile("allocators/kmem/src/slab.rs", lines: ((435, 438), (457, 459), (462, 468), (479, 482),), notes: ((437, [The source stays borrowed, exclusively, for as long as the cache lives.]), (466, [The one place that vouches for the source: it is the one this cache was bound to.]),),)[```rust
pub struct BoundCache<'a> {
    cache: Cache,
    pages: &'a mut dyn PageSource,
}
// …
    pub fn bind<'a>(self, pages: &'a mut dyn PageSource) -> BoundCache<'a> {
        BoundCache { cache: self, pages }
    }
// …
impl BoundCache<'_> {
    /// An object in its constructed state, or `None` if no object can be made.
    pub fn allocate(&mut self) -> Option<usize> {
        // SAFETY: this wrapper keeps the originating page source exclusively borrowed.
        unsafe { self.cache.allocate(self.pages) }
    }

// …
    pub fn reap(&mut self) -> usize {
        // SAFETY: this wrapper keeps the originating page source exclusively borrowed.
        unsafe { self.cache.reap(self.pages) }
    }
```]

`BoundCache` dereferences to `Cache` for the methods that only report. A `compile_fail` doc test on `bind` drops the source while the bound cache still holds it, and checks that this doesn't compile.

`moved` shifts a slab between lists when its count crosses a boundary, using `list_for`, `push` and `unlink`. The remaining methods report: `name`, `object_size`, `buffer_size`, `per_slab`, `spare` (the bytes that are neither buffers nor record), `colours`, `slabs` (full, partial, empty), `pages`, `in_use`, `stats` (`allocations`, `refused`, `frees`, `slabs_created`, `slabs_destroyed`, `constructed`, `destroyed`) and `check`, which walks every list and every slab's free list and compares them with the counts.

== How it is exercised

The 24 host tests are 12 for `frames` and 12 for `slab`. Among them, `a_random_workload_leaves_the_region_as_it_found_it` drives the buddy system through 20,000 steps of random orders and calls `check` every 500, and `a_cache_over_page_frames_gives_every_page_back` runs a `Cache` over a real `Frames`.

#output(title: "cargo test -p green-tea-kmem")[
```
test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `slab` kernel runs both layers on real page frames. In the first part, columns are the free blocks of each order:

#output(title: "cargo xtask kernel --kernel slab --target riscv64")[
```
page frames: 4096 of 4096 bytes, the 16 MiB of RAM from 0x80c00000, where the kernel ends
their table: 4096 records of 12 bytes, in the first 12 pages
  free blocks of order        0    1    2    3    4    5    6    7    8    9   10 pages free
  at boot                     0    0    1    0    1    1    1    1    1    1    3       4084
  a = allocate(0): page 12    1    1    0    0    1    1    1    1    1    1    3       4083
  b = allocate(0): page 13    0    1    0    0    1    1    1    1    1    1    3       4082
  c = allocate(3): page 16    0    1    0    1    0    1    1    1    1    1    3       4074
  free(a), of order 0         1    1    0    1    0    1    1    1    1    1    3       4075
  free(b), of order 0         0    0    1    1    0    1    1    1    1    1    3       4076
  free(c), of order 3         0    0    1    0    1    1    1    1    1    1    3       4084
  3 blocks halved, 3 pairs of buddies merged
  free(c) again: Err(NotAllocated)
```
]

The 12 pages of the table leave page 12 as the first free page, so at boot the region is a block of 4 pages at 12, one of 16 at 16, and so on up. Freeing `a`, `b` and `c` restores that line exactly.

External fragmentation is visible too. The buddy system can hold half its memory free and refuse two pages:

#output(title: "the same run")[
```
every page allocated on its own, 4084 of them, and written:
  every other one freed    2042    0    0    0    0    0    0    0    0    0    0       2042
  2042 pages free, 2042 in use; a request for two pages together: refused
  the rest freed              0    0    1    0    1    1    1    1    1    1    3       4084
  4074 blocks halved on the way down, 4074 pairs merged on the way back
```
]

The second part is the slab cache, on objects of 200 bytes:

#output(title: "the same run")[
```
a cache of objects of 200 bytes: a buffer is 208 bytes, the object and then a link
a slab is a page: 19 buffers, a record of 40 bytes at the end, and 104 bytes to spare
  slabs:                     full partial empty objects constructed destroyed pages pages free
  a new cache                   0       0     0       0           0         0     0       4084
  1 object allocated            0       1     0       1          19         0     1       4083
  19 objects                    1       0     0      19          19         0     1       4083
  40 objects                    2       1     0      40          57         0     3       4081
  all 40 freed                  0       0     3       0          57         0     3       4081
  40 objects again              2       1     0      40          57         0     3       4081
  all freed, and reaped         0       0     0       0          57        57     0       4084
the first buffer of each of 16 new slabs, as an offset in its page (14 colours):
  0 8 16 24 32 40 48 56 64 72 80 88 96 104 0 8
```
]

The second 40 allocations construct nothing: the count stays at 57. A slab pinned by one object can't be reaped, so where the survivors sit decides how much comes back:

#output(title: "the same run")[
```
190 objects in 10 slabs, all freed but 10, one in each slab: slabs 10 partly used, 0 empty
  reaped, the cache gives back 0 pages and keeps 10
190 objects in 10 slabs, all freed but 10, all in one slab: slabs 1 partly used, 9 empty
  reaped, the cache gives back 9 pages and keeps 1
```
]

The kernel's own task records come from such a cache, and a constructed record holds a stack from the pool of @crate-pool, so a task reused from the cache needs no stack allocation:

#output(title: "the same run")[
```
the kernel's cache of task records: a record is 512 bytes, a buffer 520, and a slab holds 7
  a constructed record has a stack, of which the pool has 102
1000 tasks, one after another, every one with its locals at 0x80a09f2e:
  1001 records allocated, 7 constructed, in 1 slab; 7 stacks taken from the pool
```
]

== Limits

- Neither layer is thread-safe. `green-tea-bare` calls them with interrupts off.
- A slab is one page. Bonwick's layout for objects larger than an eighth of a page, with the records kept elsewhere and slabs of several pages, isn't here, so a large object wastes much of its slab, and an object that doesn't fit beside its link and the record makes `Cache::new` panic.
- The buddy system fragments externally, and rounds every request up to a power of two pages.
- `free` of a slab object can't tell an address from another cache's slab, or from a page that was never a slab. Its signature is `unsafe` for that reason.
- A cache given a destructor and no constructor runs the destructor at `reap` on objects nothing ever constructed.

Pages from a table, objects from pages. @crate-region allocates without freeing anything one block at a time.
// chapter-status:begin
// stage: research-done
// revision: 3
// body: 607688325196
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
