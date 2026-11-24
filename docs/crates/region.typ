#import "../lib.typ": *

= green-tea-region <crate-region>

`green-tea-region` allocates memory a block at a time and frees all of it at once. A `Region` is a bump allocator over a list of *chunks* from the global allocator, and the borrow checker enforces its lifetimes: an object allocated from a region can't be used after the region is reset or dropped. The shell's commands run in one.

#spec(
  ("Source", [#repo("green-tea-region/src/lib.rs"), 352 lines without its tests; #repo("green-tea-region/examples/cost.rs"), 115 lines]),
  ("Depends on", [`alloc`; `no_std`. `green-tea-fit` for the `cost` example only]),
  ("Used by", [`green-tea-shell`]),
  ("Tests", [15 host tests, 5 doc tests that must fail to compile, the `cost` example, the `region` kernel]),
)

== Public interface

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Region::new(chunk_size)`], [a region that takes chunks of that size, and none until it is first used. A `const fn`],
  [`alloc(value)`], [moves a value in and returns `&mut T`. Refuses, at compile time, a type with a destructor],
  [`alloc_slice(items)`], [a copy of a slice of `Copy` values],
  [`alloc_str(text)`], [a copy of a string],
  [`reset()`], [frees everything, keeps the chunk being filled. Takes `&mut self`],
  [`counts()`], [the `Counts` below],
  [`Allocator` impl], [so that `Vec::new_in(&region)` grows inside the region],
  [`scope(chunk_size, body)`], [runs `body` with a region of its own, freed when it returns],
  [`HEADER`], [the bytes of a chunk that its header takes: 16 on a 64-bit machine],
)

`Counts` holds `blocks` (handed out since the region was made), `used` and `peak` (bytes since the last reset, with padding), `chunks` and `held` (what the region holds now, headers included), `taken` (chunks ever taken from the global allocator), `most_chunks` and `resets`.

== Lifetimes in the types

`alloc` takes `&self` and returns a `&mut T` tied to that borrow:

#srcfile("green-tea-region/src/lib.rs", lines: ((158, 166),), notes: ((159, [A *const* assertion, evaluated for each `T` when the program is built. A region drops nothing inside it, so a `String` here would leak its buffer, and the build fails instead.]),))[```rust
pub fn alloc<T>(&self, value: T) -> &mut T {
    const { assert!(!mem::needs_drop::<T>(), "a region runs no destructors, and this type has one") };
    let block = self.block(Layout::new::<T>()).cast::<T>();
    // SAFETY: the block is new, large enough for a T and aligned for one.
    unsafe {
        block.write(value);
        &mut *block.as_ptr()
    }
}
```]

Three rules follow, and each is a `compile_fail` doc test in the crate:

- An object can't outlive its region. Using it after the block that owns the region ends is error `E0597`.
- `reset` takes `&mut self`, which can't be had while anything allocated from the region is borrowed (`E0502`).
- With two regions, an object in the one that lives longer can't point into the other. The reverse is allowed.

The two remaining doc tests refuse a `String` in `alloc`, and a value escaping from `scope`. `scope` is Tofte and Talpin's `letregion`, and its signature does the work:

#srcfile("green-tea-region/src/lib.rs", lines: ((348, 351),), notes: ((348, [`for<'r>`: the body must work for a region of *any* lifetime, so its result type `R` can't mention this one.]),))[```rust
pub fn scope<R>(chunk_size: usize, body: impl for<'r> FnOnce(&'r Region) -> R) -> R {
    let region = Region::new(chunk_size);
    body(&region)
}
```]

== Chunks and the bump

Every chunk starts with a header, which links it to the chunk filled before it:

#srcfile("green-tea-region/src/lib.rs", lines: ((83, 86), (112, 121),))[```rust
struct Chunk {
    older: *mut Chunk,
    size: usize,
}
// …
pub struct Region {
    /// The size of the chunks it takes. A block too large for one gets a chunk of its own.
    chunk_size: usize,
    /// The chunk being filled, which is the head of a list of all of them.
    newest: Cell<*mut Chunk>,
    /// The first address of that chunk not handed out, and the chunk's end.
    next: Cell<usize>,
    end: Cell<usize>,
    counts: Cell<Counts>,
}
```]

#bytefield(caption: [A chunk, aligned to 16],
  ("older", "8", 1), ("size", "8", 1), ("blocks, each aligned as it asks", "…", 5), ("next → unused", "…", 2))

The fields are `Cell`s because allocation goes through `&self`. That also makes `Region` neither `Sync` nor, through its raw pointer, `Send`.

An allocation rounds `next` up, and takes a new chunk if the block doesn't fit:

#srcfile("green-tea-region/src/lib.rs", lines: ((221, 244),), notes: ((223, [A block of no size needs no memory: a dangling, aligned address serves, and the count of blocks doesn't move.]), (228, [After `take_chunk`, the second `fit` can fail only if the arithmetic overflowed.]),))[```rust
fn raw(&self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
    if layout.size() == 0 {
        return Ok(layout.dangling_ptr()); // nothing to hand out: any aligned address will do
    }
    let addr = match self.fit(layout) {
        Some(addr) => addr,
        None => {
            self.take_chunk(layout)?;
            self.fit(layout).ok_or(AllocError)?
        }
    };
    let mut counts = self.counts.get();
    counts.blocks += 1;
    self.counts.set(counts);
    self.bump_to(addr + layout.size());
    NonNull::new(addr as *mut u8).ok_or(AllocError)
}

/// Where a block of `layout` would go in the chunk being filled, if it fits there.
#[inline]
fn fit(&self, layout: Layout) -> Option<usize> {
    let addr = align_up(self.next.get(), layout.align())?;
    (addr.checked_add(layout.size())? <= self.end.get()).then_some(addr)
}
```]

`take_chunk` allocates `max(chunk_size, HEADER + size + align)`, rounded up to 16, so a block too large for a chunk gets one of its own. The new chunk becomes the one being filled, and what was left of the old one is not used again.

`reset` keeps the newest chunk and frees the rest:

#srcfile("green-tea-region/src/lib.rs", lines: ((188, 204),))[```rust
pub fn reset(&mut self) {
    let newest = self.newest.get();
    let mut counts = self.counts.get();
    counts.resets += 1;
    counts.used = 0;
    if !newest.is_null() {
        // SAFETY: `newest` is a chunk of ours, and `&mut self` says nothing borrows
        // from any of them.
        unsafe {
            Self::free_chunks((*newest).older);
            (*newest).older = ptr::null_mut();
            (counts.chunks, counts.held) = (1, (*newest).size);
        }
        self.next.set(newest as usize + HEADER);
    }
    self.counts.set(counts);
}
```]

`free_chunks` walks the list by `older`, and `Drop` calls it on the whole list. Neither looks inside a chunk, which is why a region can't run destructors.

== The `Allocator` implementation

A region implements `core::alloc::Allocator`, so a collection can grow in one. Most frees do nothing, and the last block is a special case in both directions:

#srcfile("green-tea-region/src/lib.rs", lines: ((323, 341),), notes: ((324, [Only the last block can be given back: `next` moves down again.]), (333, [The last block grows where it is, if its chunk has room. A `Vec` that is the most recent allocation pays no copy.]), (337, [Any other block is copied, and the old one stays until the region is freed.]),), size: 6.3pt)[```rust
unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
    if self.is_last(ptr, layout.size()) {
        self.bump_to(ptr.as_ptr() as usize);
    }
}

/// The last block grows where it is, if its chunk has room. Any other block is copied
/// to a new one, and the old one stays until the region is freed.
unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
    let addr = ptr.as_ptr() as usize;
    if self.is_last(ptr, old.size()) && addr.is_multiple_of(new.align()) && addr + new.size() <= self.end.get() {
        self.bump_to(addr + new.size());
        return Ok(NonNull::slice_from_raw_parts(ptr, new.size()));
    }
    let block = self.allocate(new)?;
    // SAFETY: both blocks are at least `old.size()` bytes, and the new one is new.
    unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), block.as_ptr().cast(), old.size()) };
    Ok(block)
}
```]

== How it is exercised

#output(title: "cargo test -p green-tea-region")[
```
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `cost` example runs green-tea-shell's `echo` command, a copy of the line, a vector of its words and the text it prints, each in a growing `Vec`, with its memory from five places: `system` (the C library's `malloc`), `fit` (first fit, address order), `tlsf`, `region` (one region, reset after every command) and `scope` (a new region for every command).

#srcfile("green-tea-region/examples/cost.rs", lines: ((59, 75),))[```rust
fn command<A: Allocator + Copy>(text: &str, memory: A) -> usize {
    let mut line = Vec::new_in(memory);
    line.extend_from_slice(text.as_bytes());
    let line = core::str::from_utf8(&line).unwrap_or("");

    let mut words = Vec::new_in(memory);
    words.extend(line.split_ascii_whitespace());

    let mut out = Text(Vec::new_in(memory));
    if let ["echo", rest @ ..] = words.as_slice() {
        for (i, word) in rest.iter().enumerate() {
            let _ = write!(out, "{}{word}", if i == 0 { "" } else { " " });
        }
        let _ = out.write_str("\n");
    }
    black_box(&out.0).len()
}
```]

Counted with `valgrind --tool=callgrind`, on a release build, once with no commands and once with 100,000:

#output(title: "valgrind --tool=callgrind target/release/examples/cost <way> <count>; instructions")[
```
system 0 360011
system 100000 567106463
fit 0 360100
fit 100000 575860245
tlsf 0 364041
tlsf 100000 665264214
region 0 360042
region 100000 415860699
scope 0 360039
scope 100000 460960194
```
]

Subtracting the empty run, a command costs 5,667 instructions with `malloc`, 5,755 with first fit, 6,649 with TLSF, 4,155 with a region reset each time, and 4,606 with a new region each time. The command's own work is the same in all five, so the differences are the allocators': a region reset each time saves 1,512 instructions a command over `malloc`.

The `region` kernel runs a script of the shell's commands four ways, and compares what each asks of the kernel's heap:

#output(title: "cargo xtask kernel --kernel region --target riscv64", size: 6.8pt)[
```
the session's script, 1970 commands, run four times with the heap recording:
  a command's memory    the variables   allocations      frees  bytes asked for  peak live  footprint
  the heap              the heap              10316      10316           656120      14902      21456
  the arena             the heap               2996       2996            99209      14563      20576
  a region, reset       a region                 16         16            65536      65536      65792
  a region, new         a region               1985       1985          8130560      65536      65792
the command's region: 1970 resets, 1 chunk of 4096 bytes taken from the heap, at most 436 bytes of it in use
the session's region: 15 chunks of 4096 bytes, 57693 bytes in use at the end in 1780 blocks
the variables: 74 set at the end, in 6609 of those bytes; the most ever set at once needed 10650
```
]

Two regions turn 10,316 heap allocations into 16. The price is in the session's region: it has 57,693 bytes in use at the end, in 15 chunks of 4 KiB, for variables that need 6,609, because a variable set again leaves its old value behind until the region goes.

== Limits

- Nothing is freed on its own, apart from the last block given back through `Allocator`. A long-lived region with changing contents only grows, as the kernel's session region shows.
- When a block doesn't fit, the rest of the current chunk is abandoned, even if later, smaller blocks would have fit.
- `reset` keeps the newest chunk whatever its size, so one oversized block leaves an oversized chunk in the region.
- A region isn't for use from an interrupt handler or from two tasks that preempt each other: the bump is a plain load and store.
- `alloc` refuses types with destructors, but a `Vec` in a region is an ordinary `Vec`: `Vec::leak` on one of `String`s compiles, and those strings are never dropped.

Everything at once, and the types to prove it. @crate-smp makes an allocator usable from several CPUs.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: 95916d5a98f8
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
