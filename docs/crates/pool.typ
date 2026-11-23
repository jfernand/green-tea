#import "../lib.typ": *

= green-tea-pool <crate-pool>

A *pool* allocates blocks of one size from a region cut into equal *slots*, and its only state is which slots are free. `green-tea-pool` keeps that state in two ways behind one trait: `Bitmap`, one bit per slot, held apart from the slots, and `FreeList`, a linked list threaded through the free slots themselves. The kernels' task stacks come from a `FreeList`.

#spec(
  ("Source", [#repo("green-tea-pool/src/lib.rs"), 159 lines without its tests]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `stack` module), `green-tea-measure`]),
  ("Tests", [9 host tests, the `interface` example, the `pool` kernel]),
)

== The `Pool` trait

#srcfile("green-tea-pool/src/lib.rs", lines: ((24, 24), (36, 36), (39, 39), (46, 46), (49, 50),))[```rust
pub trait Pool: Sized {
// …
    unsafe fn new(base: usize, slot_size: usize, slots: usize, link_at: usize) -> Self;
// …
    fn take(&mut self) -> Option<usize>;
// …
    unsafe fn give(&mut self, slot: usize) -> Result<(), GiveError>;
// …
    fn free(&self) -> usize;
}
```]

`new` is `unsafe` because the pool takes the caller's word that the region at `base` belongs to it. `link_at` is the offset, inside a free slot, where a pool may keep a word of its own. `FreeList` needs it because a slot can't always give up its first word: a task stack's lowest bytes are a guard page, so the link has to sit higher.

`give` is `unsafe` too, and returns a `GiveError`, either `NotASlot` or `NotTaken`. The contract says a pool reports the mistakes it can detect, and need not detect any. The two implementations sit at opposite ends of that sentence.

== `Bitmap`

`Bitmap` holds up to 128 slots, one bit each in a `u128`, with 1 meaning free. `take` finds the lowest set bit:

#srcfile("green-tea-pool/src/lib.rs", lines: ((73, 80),))[```rust
fn take(&mut self) -> Option<usize> {
    if self.free == 0 {
        return None;
    }
    let slot = self.free.trailing_zeros() as usize;
    self.free &= !(1 << slot);
    Some(self.base + slot * self.slot_size)
}
```]

`give` works the index back out of the address, and so it can refuse an address that isn't the start of a slot, or a slot whose bit is already set:

#srcfile("green-tea-pool/src/lib.rs", lines: ((83, 94),))[```rust
unsafe fn give(&mut self, slot: usize) -> Result<(), GiveError> {
    let offset = slot.checked_sub(self.base).ok_or(GiveError::NotASlot)?;
    let index = offset / self.slot_size;
    if index >= self.slots || !offset.is_multiple_of(self.slot_size) {
        return Err(GiveError::NotASlot);
    }
    if self.free & (1 << index) != 0 {
        return Err(GiveError::NotTaken);
    }
    self.free |= 1 << index;
    Ok(())
}
```]

The bitmap never touches the slots' memory. A program that writes to a free slot can't damage the pool.

== `FreeList`

`FreeList` holds any number of slots, and all its state fits in seven words: the region's shape, the list's `head`, a counter `fresh` and the number free. A free slot's word at `link_at` holds the address of the next free slot, so the list is *intrusive*.

#srcfile("green-tea-pool/src/lib.rs", lines: ((131, 145),), notes: ((137, [`fresh` counts the slots that were never taken. They aren't on the list, so `new` doesn't have to write a link into every slot, and making a pool touches none of its slots.]),))[```rust
fn take(&mut self) -> Option<usize> {
    let slot = if self.head != 0 {
        let slot = self.head;
        // The slot is free, so the word at `link_at` is ours: the next free slot.
        self.head = unsafe { ((slot + self.link_at) as *const usize).read() };
        slot
    } else if self.fresh < self.slots {
        self.fresh += 1;
        self.base + (self.fresh - 1) * self.slot_size
    } else {
        return None;
    };
    self.free -= 1;
    Some(slot)
}
```]

`give` is three lines, and checks nothing:

#srcfile("green-tea-pool/src/lib.rs", lines: ((148, 153),))[```rust
unsafe fn give(&mut self, slot: usize) -> Result<(), GiveError> {
    unsafe { ((slot + self.link_at) as *mut usize).write(self.head) };
    self.head = slot;
    self.free += 1;
    Ok(())
}
```]

A slot given back twice ends up linked to itself, and from then on the pool hands that slot to every caller. The crate has a test that pins this behaviour down, `a_free_list_given_a_slot_twice_hands_it_out_twice`.

== Behaviour at the edges

The `interface` example runs both pools through the same calls, on four slots of 64 bytes:

#output(title: "cargo run -p green-tea-pool --example interface")[
```
Bitmap
  take(), 5 times          = Some(slot 0), Some(slot 1), Some(slot 2), Some(slot 3), None
  give(slot 2)             = Ok(())
  give(slot 2), again      = Err(NotTaken)
  free()                   = 1
  take(), twice            = Some(slot 2), None
FreeList
  take(), 5 times          = Some(slot 0), Some(slot 1), Some(slot 2), Some(slot 3), None
  give(slot 2)             = Ok(())
  give(slot 2), again      = Ok(())
  free()                   = 2
  take(), twice            = Some(slot 2), Some(slot 2)
```
]

The free list reports two free slots when one is free, and gives the same slot to two callers.

The order of reuse differs as well. After slots 2, 5 and 0 are given back, a `Bitmap` hands out 0, 2, 5, the lowest first, and a `FreeList` hands out 0, 5, 2, the last one given back first. The `pool` kernel shows both on all three architectures, then runs 1,000 tasks one after another on stacks from the pool:

#output(title: "cargo xtask kernel --kernel pool --target riscv64")[
```
a pool of 8 slots:        bitmap                     free list
  taken from new:          [0, 1, 2, 3, 4, 5, 6, 7]   [0, 1, 2, 3, 4, 5, 6, 7]
  a ninth take refused:    true                       true
  after giving 2, 5, 0:    [0, 2, 5]                  [0, 5, 2]
  7 given back twice:      Err(NotTaken)              Ok(())
  the next two takes:      [Some(7), None]            [Some(7), Some(7)]
PASS: 1000 tasks ran on one stack; the bitmap caught a double give, the free list didn't
```
]

All 1,000 tasks had their locals at the same address, because each reused the stack the previous one gave back.

== Limits

- `Bitmap` panics on more than 128 slots.
- `FreeList` can't tell a foreign address or a double give from a correct one.
- Address 0 means an empty list, so a `FreeList` can't manage a region that starts at address 0.
- Neither pool is thread-safe. The kernel's stack pool is used from the boot CPU only.

Two pools, and one difference: where the bookkeeping lives. @crate-fit drops the requirement that every block be the same size.
// chapter-status:begin
// stage: early-draft
// revision: 1
// body: 25b781e1ffc8
// activity: 2026-11-22 | r1 | stage | green-tea | note | tracking started at early-draft
// chapter-status:end
