#import "../lib.typ": *

= green-tea-fit <crate-fit>

`green-tea-fit` allocates blocks of any size from one region of memory. It holds four allocators: `Fit`, a free list searched by first, next or best fit; `Buddy`, a binary buddy system; `Tlsf`, two-level segregated fit, which is the kernels' heap; and `Hardened`, `Fit` with five switchable defences. Around them are the tools that measure them: trace replay, a trace file format, a page accounting module, and an adversary program written to fragment a heap.

#spec(
  ("Source", [#repo("green-tea-fit/src/"), 8 files, 2,786 lines without their tests]),
  ("Depends on", [nothing; `no_std`. `trace`, `tracefile` and `adversary` need `alloc`]),
  ("Used by", [`green-tea-bare`, `green-tea-smp`, `green-tea-shard`, `green-tea-trace`, `green-tea-measure`]),
  ("Tests", [75 host tests, the `interface` example, the `fit`, `frag`, `tlsf` and `safety` kernels]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [crate root], [`Fit`, `Policy`, `Order`, the `Heap` trait, `Stats`, `Block`, `FreeError`, `ALIGN`, `OVERHEAD`, `MIN_BLOCK`],
  [`buddy`], [`Buddy`, with `MIN_BLOCK` and `ORDERS` of its own],
  [`tlsf`], [`Tlsf`, `class`, `class_for`, `range`, `RANGES`, `SMALL`, `ROWS`],
  [`hardened`], [`Hardened<D>`, `Fault`, `Counts`, the defence bits, `POISON`, `QUARANTINE_MAX`, `QUARANTINE_DEFAULT`],
  [`trace`], [`Event`, `Report`, `replay`, `lifetimes`, `shuffled`, `by_lifetime`, `Random`],
  [`tracefile`], [`Record`, `TextWriter`, `read`, `read_text`, `read_binary`, `write_text`, `write_binary`, `checksum`, `events`],
  [`pages`], [`idle`, `idle_ranges`, `spanned`],
  [`adversary`], [`run`, `Round`, `block_size`],
)

== The `Heap` trait

All four allocators implement `Heap`, which is what a replay needs from an allocator:

#srcfile("green-tea-fit/src/lib.rs", lines: ((130, 130), (132, 132), (137, 137), (139, 139), (142, 143), (145, 146),))[```rust
pub trait Heap {
// …
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize>;
// …
    unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError>;
// …
    fn used(&self) -> usize;
// …
    fn footprint(&self) -> usize;
    fn stats(&self) -> Stats;
// …
    fn check(&self) -> Result<(), &'static str>;
}
```]

The *footprint* is the distance from the start of the region to the highest address an allocated block has reached. It never goes down, and a region of that size would have been enough for everything asked so far. `used` counts the bytes in allocated blocks, with tags and rounding.

`Stats` is the same record for all of them: `allocations`, `failed`, `frees`, `examined` (free blocks looked at while searching), `passed` (blocks walked past to find a freed block's place), `splits`, `merges`, `peak`, `longest_search` and `longest_walk`. `free` refuses an address with a `FreeError`: `NotABlock`, `NotAllocated`, or `Corrupt` when the tags don't make sense.

== `Fit`: a searched free list

=== Block layout

Every block starts with a word, its *header*, and ends with a word, its *footer*. Both hold the block's size with the lowest bit set if it is allocated. These are Knuth's *boundary tags*. A free block also keeps two links, in its second and third words:

#bytefield(caption: [An allocated block],
  ("size|1", "8 bytes", 1), ("the caller's bytes", "size − 16", 4), ("size|1", "8 bytes", 1))
#bytefield(caption: [A free block],
  ("size", "8", 1), ("next", "8", 1), ("prev", "8", 1), ("unused", "size − 32", 2), ("size", "8", 1))

#srcfile("green-tea-fit/src/lib.rs", lines: ((55, 55), (57, 57), (59, 59),))[```rust
pub const ALIGN: usize = 2 * WORD;
// …
pub const OVERHEAD: usize = 2 * WORD;
// …
pub const MIN_BLOCK: usize = 4 * WORD;
```]

Addresses handed out are multiples of 16, so a header sits 8 bytes past a multiple of 16, every size is a multiple of 16, and the lowest four bits of a tag are free for flags. The smallest block is 32 bytes. Beyond the last block, `new` writes one word marked allocated, and it marks the word before the first block the same way, so that merging never runs off either end of the region.

=== Allocation

#srcfile("green-tea-fit/src/lib.rs", lines: ((213, 213), (216, 216), (218, 253),), notes: ((216, [What the block must hold: the request and two tags, rounded up to 16, at least 32. `checked_add` makes a request near `usize::MAX` fail instead of wrapping.]), (229, [If what is left couldn't be a block, the new block takes it, as internal fragmentation.]), (237, [Only for an alignment above 16: the start of the free block stays free, in its place on the list.]),))[```rust
pub fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
// …
    let need = size.checked_add(OVERHEAD + ALIGN - 1).map(|n| (n & !(ALIGN - 1)).max(MIN_BLOCK));
// …
    let found = need.and_then(|need| Some((need, self.find(need, align)?)));
    self.stats.longest_search = self.stats.longest_search.max(self.stats.examined - examined);
    let Some((need, (block, start))) = found else {
        self.stats.failed += 1;
        return None;
    };
    // The new block runs from `start` to `end`. It takes the rest of the free block
    // too if the rest couldn't be a block of its own.
    let block_end = block + self.size(block);
    let mut end = start + need;
    if block_end - end < MIN_BLOCK {
        end = block_end;
    }
    if end < block_end {
        self.set_block(end, block_end - end, false);
        self.stats.splits += 1;
    }
    if start > block {
        // Only when `align` is more than 16: the free block's beginning stays free,
        // in its place on the list, and what is left after the new block is another.
        self.set_block(block, start - block, false);
        if end < block_end {
            self.insert(end);
        }
    } else if end < block_end {
        self.replace(block, end); // what is left takes the free block's place
    } else {
        self.unlink(block);
    }
    self.set_block(start, end - start, true);
    self.used += end - start;
    self.high = self.high.max(end);
    self.stats.allocations += 1;
    self.stats.peak = self.stats.peak.max(self.used);
    Some(start + WORD)
}
```]

`find` picks the free block according to the `Policy`. First fit stops at the first block with room. Next fit does the same starting from a *rover*, the place the last search stopped, and wraps around once. Best fit goes through the list keeping the smallest block with room, and stops early on an exact match:

#srcfile("green-tea-fit/src/lib.rs", lines: ((433, 449),))[```rust
Policy::BestFit => {
    let (mut block, mut best) = (self.head, None);
    while block != 0 {
        self.stats.examined += 1;
        let size = self.size(block);
        if best.is_none_or(|(_, _, smallest)| size < smallest)
            && let Some(start) = place(block, size, need, align)
        {
            best = Some((block, start, size));
            if size == need {
                break; // none fits better
            }
        }
        block = self.next(block);
    }
    best.map(|(block, start, _)| (block, start))
}
```]

`place` answers "does it fit here, and where". For an alignment above 16 the start may have to move up, and the bytes it leaves behind must be a block themselves, at least 32 bytes:

#srcfile("green-tea-fit/src/lib.rs", lines: ((554, 560),))[```rust
pub(crate) fn place(block: usize, size: usize, need: usize, align: usize) -> Option<usize> {
    let mut start = (block + WORD).next_multiple_of(align) - WORD;
    if start != block && start - block < MIN_BLOCK {
        start += align;
    }
    (start + need <= block + size).then_some(start)
}
```]

=== Freeing

`free` trusts nothing it can check. The address must be a multiple of 16 inside the region, the block must be allocated, its size must make sense, and its two tags must agree. Then the footer of the block before it and the header of the block after it, each one word away, say whether a neighbour is free:

#srcfile("green-tea-fit/src/lib.rs", lines: ((281, 300),), notes: ((281, [The block is marked free before merging, so that a second `free` of the same address finds a free tag for as long as nothing overwrites it.]), (283, [One word below this block is the previous block's footer: its size, and whether it is free.]),))[```rust
self.set_block(block, size, false);

let (before, before_used) = self.tag(block - WORD); // the footer of the block before
if !before_used {
    block -= before;
    size += before;
    self.unlink(block);
    self.stats.merges += 1;
}
let (after, after_used) = self.tag(block + size); // the header of the block after
if !after_used {
    self.unlink(block + size);
    size += after;
    self.stats.merges += 1;
}
self.set_block(block, size, false);
let passed = self.stats.passed;
self.insert(block);
self.stats.longest_walk = self.stats.longest_walk.max(self.stats.passed - passed);
Ok(())
```]

Merging takes constant time. Putting the result on the list depends on the `Order`: `Lifo` pushes it on the front, and `Address` walks the list to keep it sorted, counting every block passed in `Stats::passed`.

`Fit` also offers `blocks` and `free_list` (iterators over `Blocks` and `FreeBlocks`), `capacity`, `first`, `largest_free`, and `check`, which walks the region and then the list and verifies every invariant: tags agree, no two free blocks are neighbours, `used` adds up, every link has a matching back link, and the list holds exactly the free blocks.

== `Buddy`: the binary buddy system

Every block in `Buddy` is a power of two, from `MIN_BLOCK` (32 bytes) up, and is only ever split in half or merged with the half it was split from. There is one free list per size, up to `ORDERS` (27), which is 2 GiB. A block starts with one word holding its order and an allocated bit, so a request for 100 bytes needs 108 and takes 128.

`order_for` rounds a request up:

#srcfile("green-tea-fit/src/buddy.rs", lines: ((60, 63),))[```rust
pub fn order_for(size: usize) -> Option<usize> {
    let block = size.checked_add(WORD)?.checked_next_power_of_two()?.max(MIN_BLOCK);
    Some((block / MIN_BLOCK).ilog2() as usize)
}
```]

Allocation takes the smallest non-empty list at or above that order, and halves the block until it is the right size, leaving each upper half free:

#srcfile("green-tea-fit/src/buddy.rs", lines: ((84, 97),))[```rust
let block = self.heads[at];
self.unlink(block, at);
// Halve it until it is the size wanted. The upper half of each cut stays free.
while at > order {
    at -= 1;
    self.push(block + (MIN_BLOCK << at), at);
    self.stats.splits += 1;
}
self.write(block, order << 1 | USED);
self.used += MIN_BLOCK << order;
self.high = self.high.max(block + (MIN_BLOCK << order));
self.stats.allocations += 1;
self.stats.peak = self.stats.peak.max(self.used);
Some(block + WORD)
```]

A block of size $s$ is at an offset that is a multiple of $s$, so its buddy is at that offset with one bit flipped. Freeing finds the only block it could merge with by an exclusive or, and reads one word:

#srcfile("green-tea-fit/src/buddy.rs", lines: ((123, 135),))[```rust
while order < self.top {
    let buddy = self.base + ((block - self.base) ^ (MIN_BLOCK << order));
    // The buddy can be merged only if it is free and hasn't been split.
    if self.read(buddy) != order << 1 {
        break;
    }
    self.unlink(buddy, order);
    block = block.min(buddy);
    order += 1;
    self.stats.merges += 1;
}
self.push(block, order);
Ok(())
```]

The comparison on line 126 tests two facts at once. A buddy's first word equals `order << 1` only if the buddy is free and of the same order, which means it hasn't been split.

`Buddy` refuses any alignment above 16, as the `interface` example shows below: `allocate(24, 64)` is `None`.

== `Tlsf`: two-level segregated fit

`Tlsf` has `Fit`'s blocks and replaces the single list with a table of lists and two bitmaps. Sizes below `SMALL` (256) have a list each. From 256 up, each power of two is a row, cut into `RANGES` (16) equal columns: 256–271, 272–287, up to 511, then 512–543, and so on. With `ROWS` (25) rows the table holds 400 lists, and the largest block is a little under 4 GiB.

#srcfile("green-tea-fit/src/tlsf.rs", lines: ((30, 37),))[```rust
pub fn class(size: usize) -> (usize, usize) {
    if size < SMALL {
        (0, size / ALIGN)
    } else {
        let power = size.ilog2();
        ((power - SMALL_BITS + 1) as usize, (size >> (power - RANGE_BITS)) - RANGES)
    }
}
```]

`class` says which list a free block belongs on. `class_for` rounds a request up to the *start* of a range, so that every block on the chosen list is large enough and none has to be examined:

#srcfile("green-tea-fit/src/tlsf.rs", lines: ((41, 43),))[```rust
pub fn class_for(need: usize) -> (usize, usize) {
    if need < SMALL { class(need) } else { class(need + (1 << (need.ilog2() - RANGE_BITS)) - 1) }
}
```]

`find` is two masks and two counts of trailing zeros:

#srcfile("green-tea-fit/src/tlsf.rs", lines: ((186, 201),), notes: ((191, [The lists in this row at or after the wanted column.]), (193, [None: take the first non-empty row above this one, and its first column.]),))[```rust
fn find(&self, room: usize) -> Option<usize> {
    let (mut row, column) = class_for(room);
    if row >= ROWS {
        return None;
    }
    let mut columns = self.columns[row] & (!0 << column);
    if columns == 0 {
        let rows = self.rows & (!0 << (row + 1));
        if rows == 0 {
            return None;
        }
        row = rows.trailing_zeros() as usize;
        columns = self.columns[row];
    }
    Some(self.heads[row][columns.trailing_zeros() as usize])
}
```]

Nothing is searched, so `allocate` and `free` do a bounded amount of work however many blocks are free. The price is that a request can be refused while a block on the list below would have fitted, and rounding up can cost a few bytes per request. An alignment above 16 asks for `need + align + MIN_BLOCK`, so that the aligned block fits wherever the free block starts.

The `tlsf` kernel puts four allocators in front of one allocation of 4,096 bytes, with up to 8,000 free holes of 64 bytes in the way:

#output(title: "cargo xtask kernel --kernel tlsf --target riscv64")[
```
an allocation of 4,096 bytes and its free, with holes of 64 bytes in the way (100 of each, averaged):
  holes  allocator               looked at  walked past  microseconds
   8000  first fit, by address        8001         8000       4373.6
   8000  best fit, by address         8001         8000       5518.7
   8000  buddy system                    1            0          4.1
   8000  TLSF                            1            0          7.6
the kernel's heap (TLSF): 6622 allocations and 6621 frees since boot; the most blocks one allocation looked at: 1; the most one free walked past: 0
```
]

`Tlsf::lists` reports which lists have blocks; `blocks`, `free_list`, `capacity`, `first`, `largest_free` and `check` match `Fit`'s, and `check` also verifies both bitmaps against the lists.

== The interface, compared

The `interface` example runs the same calls against the three allocators. Where they agree it shows the contract, and where they differ it shows the design:

#output(title: "cargo run -p green-tea-fit --example interface")[
```
Fit (first fit, address order)
  allocate(100, 8)        = Some(region + 16), used 128
  allocate(0, 1)          = Some(region + 144), used +32
  allocate(24, 64)        = Some(region + 256), address % 64 = 0
  allocate(8192, 8)       = None
  free(a), again          = Err(NotAllocated)
  free(a + 8)             = Err(NotABlock)
Buddy
  allocate(24, 64)        = None, address % 64 = -
```
]

== `Hardened`: defences against a corrupted heap

`Hardened<D>` is `Fit`'s first fit over a LIFO list, with five defences. `D` is a set of bits and a const parameter, so each combination is compiled on its own, and `Hardened<0>::free` contains no trace of the quarantine.

#dtable(columns: (auto, 1fr), ("Bit", "Defence"),
  [`UNLINK`], [*Safe unlinking.* Before a block leaves the list, its neighbours must point back at it, and its tags must agree.],
  [`ENCODE`], [Links are stored XORed with a secret and with their own address.],
  [`CANARY`], [A secret word between the caller's bytes and the footer, checked on `free`.],
  [`QUARANTINE`], [A freed block is filled with `POISON` (the byte `0xdd`) and held, by default in `QUARANTINE_DEFAULT` (16) slots and at most `QUARANTINE_MAX` (64). The fill is checked when it leaves.],
  [`RANDOM`], [An allocation takes a random block among those that fit, and a freed block goes to a random quarantine slot.],
)

`ALL` is the five together. Calls return a `Fault` when a defence fires: `NotABlock`, `NotAllocated`, `Tags`, `Canary`, `Link`, `Unlink` or `WrittenAfterFree`. After a fault the allocator makes no further promise. Through the `Heap` trait, whose `allocate` has no way to return a fault, it panics.

Encoding and validating a link happen in one place:

#srcfile("green-tea-fit/src/hardened.rs", lines: ((567, 576),))[```rust
fn link(&self, slot: usize) -> Result<usize, Fault> {
    let mut link = self.read(slot);
    if Self::has(ENCODE) {
        link ^= self.link_key ^ slot;
    }
    if Self::has(UNLINK | ENCODE) && link != 0 && !self.plausible(link) {
        return Err(Fault::Link);
    }
    Ok(link)
}
```]

Safe unlinking is the check that the blocks on either side point back:

#srcfile("green-tea-fit/src/hardened.rs", lines: ((519, 529),))[```rust
fn neighbours(&self, block: usize) -> Result<(usize, usize), Fault> {
    let (prev, next) = (self.link(block + 2 * WORD)?, self.link(block + WORD)?);
    if Self::has(UNLINK) {
        let forward = if prev != 0 { self.link(prev + WORD)? } else { self.head };
        let back = if next != 0 { self.link(next + 2 * WORD)? } else { block };
        if forward != block || back != block {
            return Err(Fault::Unlink);
        }
    }
    Ok((prev, next))
}
```]

A block leaving quarantine is verified word by word:

#srcfile("green-tea-fit/src/hardened.rs", lines: ((444, 462),))[```rust
    let tag = self.read(block);
    let size = tag & !FLAGS;
    if tag & FLAGS != USED | HELD
        || size < MIN_BLOCK
        || size > self.end - block
        || self.read(block + size - WORD) != tag
    {
        return Err(Fault::Tags);
    }
    self.counts.verified += size / WORD - 2;
    for word in (block + WORD..block + size - WORD).step_by(WORD) {
        if self.read(word) != POISON {
            return Err(Fault::WrittenAfterFree);
        }
    }
    self.held -= size;
    self.counts.released += 1;
    self.merge(block, size)
}
```]

The rest of the interface is `set_quarantine`, `drain` (empty the quarantine, reporting the first fault), `held`, `counts` (a `Counts` of words `poisoned` and `verified`, blocks `released`, and `peak_held`), and `block_size`. The secrets come from the caller's `seed`, so they are secret only if the seed is.

The `safety` kernel makes six mistakes against the allocator, adding one defence at a time. Here is one of them, a free block's link overwritten to point at a forged block inside the region, with a back link that passes safe unlinking:

#output(title: "cargo xtask kernel --kernel safety --target riscv64")[
```
a write to a freed block: a link to a forged block inside the region, which points back:
  no defence        not caught: the second allocation is inside another block
  + safe unlinking  not caught: the second allocation is inside another block
  + encoded links   caught by the next allocation: Link
  + canary          caught by the next allocation: Link
  + quarantine      caught by the 16th free after it: WrittenAfterFree
  + random          caught by the 3rd free after it: WrittenAfterFree
```
]

The defences have a cost, measured on the same workload of 20,000 allocations and frees:

#output(title: "cargo xtask kernel --kernel safety --target riscv64")[
```
  allocator          peak used  peak held  footprint  looked at  words filled
  no defence            118528          0     201200        3.2          0.0
  + safe unlinking      118528          0     201200        3.2          0.0
  + encoded links       118528          0     201200        3.2          0.0
  + canary              121632          0     199856        2.9          0.0
  + quarantine          121632      12384     210624        3.1         56.0
  + random              121664      12816     238992      172.2         56.0
```
]

Random placement is the expensive one: it has to look at every free block to choose among those that fit, 172 per allocation against 3.

== `trace`: replaying allocations

A trace is a list of `Event`s, each an address, a size and an alignment. A size of 0 means the block at that address was freed. Addresses are the ones the program saw when it was recorded, and a replay uses them only to tell which block a free refers to.

`replay` runs a trace against any `Heap` and returns a `Report`:

#srcfile("green-tea-fit/src/trace.rs", lines: ((76, 76), (82, 100),), notes: ((84, [A free the trace never allocated is counted as `unmatched` and skipped.]), (94, [An allocation the region can't hold is counted as `failed`, and the replay goes on.]),))[```rust
pub fn replay(events: &[Event], fit: &mut (impl Heap + ?Sized)) -> Report {
// …
    for event in events {
        if event.is_free() {
            let Some((addr, size)) = blocks.remove(&event.addr) else {
                report.unmatched += 1;
                continue;
            };
            // SAFETY: `addr` came from `fit.allocate`, and leaves `blocks` as it is freed.
            unsafe { fit.free(addr) }.expect("a replayed free was refused");
            live -= size;
            report.frees += 1;
        } else {
            let size = event.size as usize;
            let Some(addr) = fit.allocate(size, event.align as usize) else {
                report.failed += 1;
                continue;
            };
            blocks.insert(event.addr, (addr, size));
            live += size;
            report.allocations += 1;
```]

`Report` holds the counts, `peak_live` (the most bytes the program asked for and held) with the `used_at_peak_live` and `footprint_at_peak_live` of that moment, `peak_used`, the final `footprint` with `live_at_peak_footprint` and `used_at_peak_footprint`, the running sums `live_sum` and `footprint_sum`, blocks `left` at the end, and the allocator's `stats`.

Three functions transform a trace. `lifetimes` gives each allocation's lifetime, counted in later allocations. `shuffled` deals the same sizes and the same lifetimes out at random, destroying what a size says about a lifetime and what one allocation says about the next; comparing it with the original measures how much a real program's structure helps an allocator. `by_lifetime` splits a trace into long-lived and short-lived blocks. `Random` is a linear congruential generator with Knuth's MMIX constants, with `below` and `shuffle`, so a seed gives the same trace on every architecture.

== `tracefile`: traces on disk

A trace file holds `Record`s: `Allocate`, `Free` or `Reallocate`. It has a text form and a binary form with the same records and the same checksum. The text form is one record per line:

#srcfile("green-tea-fit/src/tracefile.rs", lines: ((13, 18),))[```rust
//! gtt 1                       the first line: the format, and its version
//! # recorded by ...           a line that starts with # is a comment
//! a 80212340 64 8             allocated: address, size, alignment
//! f 80212340                  freed: address
//! r 80212400 80212480 128 16  reallocated: old address, new address, new size, alignment
//! end 3 c2248561940bfcf8      the last line: how many records, and their checksum
```]

The binary form starts with `MAGIC`, the bytes `GTT\x01`. Each record's first byte holds its kind in the two low bits and the base-2 logarithm of its alignment in the six high bits. Addresses are stored as differences from the last address, *zigzagged* so that small negative numbers stay small, and then written as LEB128 varints:

#srcfile("green-tea-fit/src/tracefile.rs", lines: ((296, 308),))[```rust
fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// The difference `to - from`, zigzagged.
fn zigzag(out: &mut Vec<u8>, from: u64, to: u64) {
    let delta = to.wrapping_sub(from) as i64;
    varint(out, ((delta << 1) ^ (delta >> 63)) as u64);
}
```]

The checksum is FNV-1a over every field, kept by `Checksum`, which takes a record at a time through `add`. A file without an `end` line was cut short, and the readers refuse it; every refusal is an `Error` holding a `Problem` and the line or byte `at` which it was found. `Problem` is one of `NotATrace`, `BadRecord`, `BadAlignment`, `NoEnd`, `AfterEnd`, `Count` and `Checksum`.

`TextWriter` writes the text form a record at a time, through `comment`, `record` and `end`, which is how a kernel sends a trace out of its serial port without holding it twice. `read` accepts either form. `events` turns records into replayable `Events`: a reallocation becomes a free and an allocation, an allocation of no bytes becomes one of one byte (since size 0 means free), and an allocation of 4 GiB or more is left out with its free. The result counts each case in `reallocations`, `in_place`, `empty` and `too_large`.

== `pages`: blocks and pages

An allocator works in bytes and a kernel reclaims whole pages. `idle` finds the whole pages inside a free block that hold neither its header, its links nor its footer, which are the pages that could be given back without losing anything the allocator will read:

#srcfile("green-tea-fit/src/pages.rs", lines: ((17, 23),))[```rust
pub fn idle(block: Block, page: usize) -> Range<usize> {
    debug_assert!(page.is_power_of_two() && !block.used);
    // The header and the links are the block's first three words; the footer is its last.
    let start = (block.start + 3 * WORD).next_multiple_of(page);
    let end = (block.start + block.size - WORD) & !(page - 1);
    start..end.max(start)
}
```]

`idle_ranges` applies it across every free block, and `spanned` counts how many pages a byte range touches. `Fit` and `Tlsf` lay out free blocks alike, so both can use it.

== `adversary`: a program that fragments

`adversary::run` is J. M. Robson's lower-bound argument run as a program. It never holds more than a `budget` of bytes, and asks for blocks of doubling sizes. Before each doubling it looks at where the allocator put its blocks and frees as many as it can without leaving a hole big enough for the next size:

#srcfile("green-tea-fit/src/adversary.rs", lines: ((44, 56),))[```rust
for (i, &(addr, held_size)) in held.iter().enumerate() {
    let block = addr - OVERHEAD / 2;
    // Without this block, the hole would run to the next block we hold.
    let next = held.get(i + 1).map_or(usize::MAX, |&(next, _)| next - OVERHEAD / 2);
    if next - hole >= need {
        kept.push((addr, held_size));
        hole = block + block_size(held_size);
    } else {
        // SAFETY: `addr` came from `fit.allocate` and is freed once.
        unsafe { fit.free(addr) }.expect("the adversary freed a block twice");
        live -= held_size;
    }
}
```]

The blocks it keeps pin the memory around them, so each round of larger blocks goes into memory never used before. Each `Round` records the size, the blocks and bytes kept, the blocks allocated, the live bytes and the footprint. `block_size` is the block `Fit` makes for a request. The `frag` kernel runs it against every policy with a budget of 64 KiB:

#output(title: "cargo xtask kernel --kernel frag --target riscv64")[
```
  asks for  keeps  of bytes  allocates   holds  footprint  times the budget
        16      0         0       4096   65536     131072              2.00
        32   2048     32768       1024   65536     180224              2.75
        64   2560     49168        255   65488     200624              3.06
      4096     91     32784          7   61456     388384              5.93
```
]

All six combinations of policy and order end at the same 388,384 bytes, 5.93 times the program's budget. On a random workload they differ widely:

#output(title: "cargo xtask kernel --kernel frag --target riscv64")[
```
  policy     order     peak live  peak used  footprint  over live then  over peak live  over peak used
  first fit  LIFO         108665     118528     201200          100.6%           85.2%           69.7%
  first fit  address      108665     119296     135024           24.5%           24.3%           13.2%
  next fit   LIFO         108665     118016    1048560         1421.6%          864.9%          788.5%
  best fit   address      108665     118928     133632           23.2%           23.0%           12.4%
```
]

Next fit over a LIFO list used the whole 1,024 KiB region for a program that never held more than 106 KiB.

== Limits

- None of the allocators is thread-safe. The kernel puts its `Tlsf` behind a lock (@crate-bare), and `green-tea-smp` adds per-CPU caches (@crate-smp).
- `Buddy` can't align beyond 16 bytes.
- `Tlsf` and `Buddy` refuse regions of 4 GiB or more, and `Buddy` uses only the largest power of two that fits in its region.
- The free-list allocators check what their tags can tell them. A write that leaves the tags intact goes unnoticed without `CANARY`.
- `hardened.rs` doesn't build for riscv32 (@app-issues).

Four allocators, one interface, and the instruments to compare them. @crate-kmem applies the buddy idea to page frames.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: 974a7bb876f8
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
