#import "../lib.typ": *

= green-tea-memory <crate-memory>

`green-tea-memory` describes physical memory to the code that hands it out and the code that maps it, without doing either. It has three pieces: `PhysicalRange`, a range of addresses checked once at construction; `OwnedRange`, a token that says its holder has the range to itself; and `RegionRequest`, a question about an owned range ("keep it resident", "leave it unmapped", "make it read-only") that is checked against what the board can enforce, using @crate-capabilities.

#spec(
  ("Source", [#repo("green-tea-memory/src/lib.rs"), 366 lines, 234 without its tests]),
  ("Depends on", [@crate-capabilities, with no profile feature; `no_std`]),
  ("Used by", [@crate-dma, @crate-kmem (`Frames`), `green-tea-bare` (the heap and stack regions, each task's `Stack`, the boot check)]),
  ("Tests", [6 host tests and a `compile_fail` doc test; the boot check in every kernel]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`PhysicalRange::new(start, size, alignment)`], [a nonempty half-open range, with `start` aligned and no overflow, or `RegionError::InvalidRange`],
  [`PhysicalRange::from_bounds(start, end, alignment)`], [the same from two ends],
  [`start`, `end`, `len`, `is_empty`, `as_range`], [the range's bounds, size, and a `core::ops::Range`],
  [`contains(other)`, `overlaps(other)`, `contains_address(a)`], [comparisons with another range or one address],
  [`OwnedRange<'a>`], [a unique claim on a range for the lifetime `'a`; created by `unsafe` `OwnedRange::new(range)`],
  [`range()`, `into_range()`], [the claimed range; `into_range` ends the claim and returns the range],
  [`subrange(range)`], [a claim on part of the range, borrowed from the parent],
  [`request(residency, permissions)`], [a `RegionRequest` that borrows the claim],
  [`Residency`], [`Resident`, `DemandPaged` or `Unmapped`],
  [`Permissions(u8)`], [`NONE`, `READ`, `WRITE`, `EXECUTE`, `READ_WRITE`, and `union`],
  [`RegionRequest`], [`range()`, `residency()`, `permissions()`, and `validate(capabilities)`],
  [`RegionError`], [`InvalidRange`, `Unsupported(Capability)`, `OutOfMemory`],
)

== Checked ranges

A `PhysicalRange` has private fields, so the only way to have one is through `new`, and `new` refuses an empty range, an alignment that isn't a power of two, an unaligned start, and a range that would wrap past the top of the address space. Every method after that can trust its two fields:

#srcfile("green-tea-memory/src/lib.rs", lines: ((15, 23),), notes: ((16, [An alignment of 1 accepts any start. The callers pass a page, or the stack region's 2 MiB.]), (19, [`checked_add`: `start + size` past `usize::MAX` is an error, not a small `end`.]),))[```rust
pub const fn new(start: usize, size: usize, alignment: usize) -> Result<Self, RegionError> {
    if size == 0 || alignment == 0 || !alignment.is_power_of_two() || start % alignment != 0 {
        return Err(RegionError::InvalidRange);
    }
    let Some(end) = start.checked_add(size) else {
        return Err(RegionError::InvalidRange);
    };
    Ok(Self { start, end })
}
```]

The size isn't checked against the alignment: a range from `0x1000` of `0x1800` bytes with page alignment is accepted. @crate-dma, which needs whole pages, checks the length itself.

`green-tea-bare` builds its two linker regions this way. The heap's ends come from `__heap_start` and `__heap_end`, and the stack region is `stack::REGION`, 2 MiB, from `__stacks_start`, aligned to its own size. A linker script that broke either rule would stop the kernel at its first use of the region with the message in the `expect`:

#srcfile("green-tea-bare/src/pages.rs", lines: ((99, 103),))[```rust
pub fn heap_physical_range() -> PhysicalRange {
    let start = core::hint::black_box(&raw const __heap_start as usize);
    let end = core::hint::black_box(&raw const __heap_end as usize);
    PhysicalRange::from_bounds(start, end, PAGE).expect("linker heap range must be page-aligned and
»            nonempty")
}
```]

@crate-kmem's `Frames` uses `PhysicalRange::new` to check its range at construction, and reports it with `Frames::range`.

== Ownership tokens

An `OwnedRange` is a contract, not an allocation. It holds a `PhysicalRange` and a `PhantomData<&'a mut [u8]>`, which makes the compiler treat it as a mutable borrow of memory for `'a`: it can't be copied, and nothing borrowed from it can outlive it. It has no pointer and no way to read or write the memory. Creating one is `unsafe`, and the caller promises what the type can't check:

#srcfile("green-tea-memory/src/lib.rs", lines: ((95, 106),))[```rust
/// # Safety
/// `range` must describe valid RAM that is exclusively owned by this claim for the
/// entire lifetime `'a`. No independent overlapping `OwnedRange` may exist; a child made
/// with [`subrange`](Self::subrange) is allowed only while the parent is mutably borrowed.
/// The backing allocation must not be released while this value or one of its borrowed
/// requests is live. This constructor does not make the range safe to dereference.
pub const unsafe fn new(range: PhysicalRange) -> Self {
    Self {
        range,
        _ownership: PhantomData,
    }
}
```]

`subrange` and `request` take `&mut self`, so while a child claim or a request exists the parent can't be used, and the child's lifetime ends with that borrow. The doc test on `OwnedRange` is a program that drops the owner and then uses its request; the test passes because it fails to compile.

Dropping the token, or calling `into_range`, ends the claim and frees nothing. In `green-tea-bare`, each task's `Stack` now holds an `OwnedRange<'static>` for its pool slot, guard page included. `try_take` creates it when the pool hands out a slot, `top` and `guard` read their addresses from it, and `drop` turns it back into a range with `into_range` and returns the start to the pool.

== Requests

A request pairs the owned range with how it should be mapped. `validate` answers one question: can the board's code do this? It does nothing to the page tables. A request for permissions on an `Unmapped` range is a contradiction, and is `InvalidRange`; every other refusal names the missing capability:

#srcfile("green-tea-memory/src/lib.rs", lines: ((195, 224),), size: 6.6pt, notes: ((197, [A resident range with no permissions is the identity-mapped memory every board has, so it needs nothing.]), (203, [Unmapped is how a guard page is described.]), (215, [Permissions of any kind need a board that enforces them, even `READ` alone, on a board with page tables.]),))[```rust
pub const fn validate(&self, capabilities: Capabilities) -> Result<(), RegionError> {
    match self.residency {
        Residency::Resident => {}
        Residency::DemandPaged => {
            if let Err(missing) = capabilities.require(Capability::DemandPaging) {
                return Err(RegionError::Unsupported(missing.capability));
            }
        }
        Residency::Unmapped => {
            if self
                .permissions
                .is_some()
            {
                return Err(RegionError::InvalidRange);
            }
            if let Err(missing) = capabilities.require(Capability::GuardPages) {
                return Err(RegionError::Unsupported(missing.capability));
            }
        }
    }
    if self
        .permissions
        .is_some()
    {
        if let Err(missing) = capabilities.require(Capability::MemoryProtection) {
            return Err(RegionError::Unsupported(missing.capability));
        }
    }
    Ok(())
}
```]

== The boot check

`green-tea-bare`'s `boot` calls `memory::validate_kernel_regions` after the profile check of @crate-capabilities. It describes the kernel's own memory as requests and validates each against the board: the heap as demand-paged where the board pages it and resident otherwise, the stack region as resident, and, on a board with guard pages, each of the 102 guard pages in the stack region as unmapped. It also checks that the heap and the stacks don't overlap and that both end below the end of RAM, where the board knows it:

#srcfile("green-tea-bare/src/memory.rs", lines: ((116, 142),), size: 6.6pt, notes: ((118, [The token is made only to make the request. It is dropped at the end of the function.]), (119, [The request follows the board's own flag, so on the paging boards it checks `DemandPaging`, and on `aarch64` it asks for nothing.]), (137, [`subrange` refuses a guard page outside the stack region.]),))[```rust
pub fn validate_kernel_regions() -> Result<(), RegionError> {
    let heap_range = pages::heap_physical_range();
    let mut heap = unsafe { OwnedRange::new(heap_range) };
    let heap_residency = if Board::DEMAND_PAGING { Residency::DemandPaged } else { Residency::Resident };
    heap.request(heap_residency, None).validate(Board::CAPABILITIES)?;

    let stacks = stack::region_range();
    if heap_range.overlaps(stacks) {
        return Err(RegionError::InvalidRange);
    }
    if let Some(ram_end) = Board::ram_end() {
        if stacks.end() > ram_end || heap_range.end() > ram_end {
            return Err(RegionError::InvalidRange);
        }
    }
    let mut stacks = unsafe { OwnedRange::new(stacks) };
    stacks.request(Residency::Resident, None).validate(Board::CAPABILITIES)?;

    if Board::GUARD_PAGES {
        for base in stack::guard_pages() {
            let guard = PhysicalRange::new(base, pages::PAGE, pages::PAGE)?;
            let mut guard = stacks.subrange(guard)?;
            guard.request(Residency::Unmapped, None).validate(Board::CAPABILITIES)?;
        }
    }
    Ok(())
}
```]

The 102 is `stack::SLOTS`: 2 MiB divided by a 20 KiB slot (a 4 KiB guard page and a 16 KiB stack), rounded down.

== Exercised by

The host tests check the range rules (misaligned, empty, overflowing, alignment 3), containment and overlap, that a `subrange` can't escape its parent, that a resident request needs nothing, that each other kind of request fails on an empty set with the capability it lacks, and that each succeeds on a set with exactly that capability. The doc test is the borrow check above:

#output(title: "cargo test -p green-tea-memory")[
```
test tests::ranges_check_alignment_bounds_and_overflow ... ok
test tests::ranges_validate_containment ... ok
test tests::resident_identity_regions_need_no_protection_backend ... ok
test tests::subranges_cannot_escape_the_owned_range ... ok
test tests::supported_requests_are_checked_against_exact_guarantees ... ok
test tests::unavailable_mapping_and_permission_requests_fail_explicitly ... ok
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test green-tea-memory/src/lib.rs - OwnedRange (line 79) - compile fail ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The boot check runs in every kernel, before the banner, so every recorded kernel run in this report passed it on its board.

== Limits

- Nothing here maps or allocates. A validated request is a statement that the board could honour it; the boards' paging code is unchanged, and doesn't read requests.
- `is_empty` is always `false` for a range `new` accepted, and no code returns `RegionError::OutOfMemory`.
- The boot check makes claims on the heap and the stack region while the heap allocator and the stack pool own them, which the letter of `OwnedRange::new`'s contract forbids. The claims are never dereferenced and are dropped before the function returns, and the two `unsafe` blocks carry no `SAFETY` comment saying so.
- Ownership is checked by the borrow checker only where a token is passed around. Two calls to `OwnedRange::new` on the same range compile; the `unsafe` is the only guard.

Three types and a `match`, and a kernel that can say what its memory needs before it uses it. @crate-dma hands these tokens to devices.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: 1d3961446574
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
