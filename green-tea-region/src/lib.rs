//! Regions: memory that is allocated a block at a time and freed all at once.
//!
//! A [`Region`] is a bump allocator over a list of *chunks*, which it takes from the global
//! allocator as it fills them. Nothing allocated from a region is freed on its own. All of
//! it is freed together, when the region is dropped or [`reset`](Region::reset).
//!
//! The lifetimes are in the types. [`Region::alloc`] takes `&'r self` and returns
//! `&'r mut T`, so the borrow checker knows that the object lives exactly as long as the
//! borrow of its region, and refuses a program in which it could be used after the region
//! is gone:
//!
//! ```compile_fail,E0597
//! use green_tea_region::Region;
//! let kept;
//! {
//!     let region = Region::new(4096);
//!     kept = region.alloc(7u64);
//! } // the region is dropped here, and its chunks go back to the heap
//! assert_eq!(*kept, 7);
//! ```
//!
//! A reset frees everything too, so it takes `&mut self`, which can't be had while
//! anything allocated from the region is still borrowed:
//!
//! ```compile_fail,E0502
//! use green_tea_region::Region;
//! let mut region = Region::new(4096);
//! let word = region.alloc_str("hello");
//! region.reset();
//! assert_eq!(word, "hello");
//! ```
//!
//! With two regions, an object in the one that lives longer can't point into the other:
//!
//! ```compile_fail,E0597
//! use green_tea_region::Region;
//! use core::cell::Cell;
//! struct Var<'s> { value: Cell<&'s str> }
//! let session = Region::new(4096);
//! let var = session.alloc(Var { value: Cell::new("") });
//! {
//!     let command = Region::new(4096);
//!     var.value.set(command.alloc_str("only as long as the command"));
//! }
//! assert_eq!(var.value.get(), "");
//! ```
//!
//! The other direction is fine: an object in `command` may point into `session`.
//!
//! **A region runs no destructors.** Dropping it gives its chunks back and looks at nothing
//! inside them. So `alloc` refuses, when the program is built, a type that has a
//! destructor to run:
//!
//! ```compile_fail
//! use green_tea_region::Region;
//! let region = Region::new(4096);
//! let name = region.alloc(String::from("the String's buffer would never be freed"));
//! ```
//!
//! [`scope`] is Tofte and Talpin's `letregion`: a region that exists for one call.
//!
//! ```compile_fail
//! use green_tea_region::scope;
//! let escaped = scope(4096, |region| region.alloc_str("hello"));
//! ```
//!
//! Not for use from an interrupt handler, or from two tasks that can preempt each other:
//! the bump is a plain load and store.

#![no_std]

extern crate alloc;

use alloc::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use core::alloc::{AllocError, Allocator};
use core::cell::Cell;
use core::mem;
use core::ptr::{self, NonNull};

/// What every chunk starts with: the chunk the region was filling before this one, and
/// this chunk's size, header included.
#[repr(C)]
struct Chunk {
    older: *mut Chunk,
    size: usize,
}

/// The bytes of a chunk that its header takes.
pub const HEADER: usize = mem::size_of::<Chunk>();
/// What a chunk is aligned to. An object that needs more gets padding inside its chunk.
const CHUNK_ALIGN: usize = 16;

/// What a region has done and holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Blocks handed out since the region was made, resets or no.
    pub blocks: usize,
    /// Bytes in the blocks handed out since the last reset, with the padding between them.
    pub used: usize,
    /// The most `used` has been.
    pub peak: usize,
    /// Chunks the region holds now, and their bytes, headers included.
    pub chunks: usize,
    pub held: usize,
    /// Chunks the region has taken from the global allocator since it was made.
    pub taken: usize,
    /// The most chunks it has held at once.
    pub most_chunks: usize,
    pub resets: usize,
}

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

/// `addr` rounded up to a multiple of `align`, a power of two. `None` if that overflows.
fn align_up(addr: usize, align: usize) -> Option<usize> {
    Some(addr.checked_add(align - 1)? & !(align - 1))
}

impl Region {
    /// A region that will take chunks of `chunk_size` bytes. It takes the first one when
    /// it is first allocated from, so an unused region costs nothing.
    pub const fn new(chunk_size: usize) -> Region {
        Region {
            chunk_size,
            newest: Cell::new(ptr::null_mut()),
            next: Cell::new(0),
            end: Cell::new(0),
            counts: Cell::new(Counts {
                blocks: 0,
                used: 0,
                peak: 0,
                chunks: 0,
                held: 0,
                taken: 0,
                most_chunks: 0,
                resets: 0,
            }),
        }
    }

    /// Move `value` into the region. The reference is good for as long as the region is
    /// borrowed, which is until it is dropped or reset.
    ///
    /// `T` must have no destructor, because none will run: see the crate's documentation.
    ///
    /// # Panics
    /// If the global allocator has no chunk to give.
    #[allow(clippy::mut_from_ref)] // the block is new: nothing else refers to it
    pub fn alloc<T>(&self, value: T) -> &mut T {
        const { assert!(!mem::needs_drop::<T>(), "a region runs no destructors, and this type has one") };
        let block = self.block(Layout::new::<T>()).cast::<T>();
        // SAFETY: the block is new, large enough for a T and aligned for one.
        unsafe {
            block.write(value);
            &mut *block.as_ptr()
        }
    }

    /// A copy of `items` in the region.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_slice<T: Copy>(&self, items: &[T]) -> &mut [T] {
        let block = self.block(Layout::for_value(items)).cast::<T>();
        // SAFETY: the block is new, and large enough and aligned for `items.len()` Ts.
        unsafe {
            ptr::copy_nonoverlapping(items.as_ptr(), block.as_ptr(), items.len());
            core::slice::from_raw_parts_mut(block.as_ptr(), items.len())
        }
    }

    /// A copy of `text` in the region.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_str(&self, text: &str) -> &mut str {
        // SAFETY: a copy of a str's bytes is a str.
        unsafe { core::str::from_utf8_unchecked_mut(self.alloc_slice(text.as_bytes())) }
    }

    /// Free everything allocated from the region. It keeps the chunk it was filling, to
    /// fill again, and gives the others back.
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

    pub fn counts(&self) -> Counts {
        self.counts.get()
    }

    /// A new block, or the allocation-error panic.
    fn block(&self, layout: Layout) -> NonNull<u8> {
        match self.raw(layout) {
            Ok(block) => block,
            Err(AllocError) => handle_alloc_error(layout),
        }
    }

    /// A new block: from the chunk being filled if it has room, and from a new chunk
    /// if not.
    #[inline]
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

    /// Move `next`, up or down, and count the bytes.
    #[inline]
    fn bump_to(&self, next: usize) {
        debug_assert!(next <= self.end.get(), "a block would run past the end of its chunk");
        let mut counts = self.counts.get();
        counts.used = counts.used + next - self.next.get();
        counts.peak = counts.peak.max(counts.used);
        self.counts.set(counts);
        self.next.set(next);
    }

    /// Take a chunk from the global allocator that a block of `layout` fits in, and make
    /// it the one being filled. What was left of the last one is not used again.
    #[cold]
    #[inline(never)]
    fn take_chunk(&self, layout: Layout) -> Result<(), AllocError> {
        let alone = HEADER.checked_add(layout.size()).and_then(|n| n.checked_add(layout.align())).ok_or(AllocError)?;
        let size = align_up(self.chunk_size.max(alone), CHUNK_ALIGN).ok_or(AllocError)?;
        let chunk_layout = Layout::from_size_align(size, CHUNK_ALIGN).map_err(|_| AllocError)?;
        // SAFETY: the size isn't zero: it is at least a header.
        let chunk = unsafe { alloc(chunk_layout) }.cast::<Chunk>();
        if chunk.is_null() {
            return Err(AllocError);
        }
        // SAFETY: the chunk is new, and starts with room for its header.
        unsafe { chunk.write(Chunk { older: self.newest.get(), size }) };
        self.newest.set(chunk);
        self.next.set(chunk as usize + HEADER);
        self.end.set(chunk as usize + size);
        let mut counts = self.counts.get();
        counts.chunks += 1;
        counts.held += size;
        counts.taken += 1;
        counts.most_chunks = counts.most_chunks.max(counts.chunks);
        self.counts.set(counts);
        Ok(())
    }

    /// Give `chunk`, and every chunk older than it, back to the global allocator.
    ///
    /// # Safety
    /// They must be chunks `take_chunk` made, with nothing in them in use, and must not be
    /// used again.
    unsafe fn free_chunks(mut chunk: *mut Chunk) {
        while !chunk.is_null() {
            // SAFETY: the caller's promise. `take_chunk` allocated each with this layout.
            unsafe {
                let Chunk { older, size } = chunk.read();
                dealloc(chunk.cast(), Layout::from_size_align_unchecked(size, CHUNK_ALIGN));
                chunk = older;
            }
        }
    }

    /// Whether the block at `ptr`, of `size` bytes, is the last one handed out.
    fn is_last(&self, ptr: NonNull<u8>, size: usize) -> bool {
        size != 0 && ptr.as_ptr() as usize + size == self.next.get()
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the region is being dropped, so nothing borrows from its chunks.
        unsafe { Self::free_chunks(self.newest.get()) };
    }
}

/// A region is an allocator, so a collection can grow in one: `Vec::new_in(&region)`. The
/// collection borrows the region, as an object made by [`Region::alloc`] does, and
/// `Vec::leak` turns it into a slice that lives as long as that borrow.
unsafe impl Allocator for Region {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Ok(NonNull::slice_from_raw_parts(self.raw(layout)?, layout.size()))
    }

    /// Only the last block can be given back before the region is freed: that moves
    /// `next` down again. Any other block stays until then.
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
}

/// Run `body` with a region of its own, which is freed when `body` returns: Tofte and
/// Talpin's `letregion`. `body` must work for a region of any lifetime, so the type of
/// what it returns can't mention this one's, and nothing allocated from the region can
/// be in the result.
pub fn scope<R>(chunk_size: usize, body: impl for<'r> FnOnce(&'r Region) -> R) -> R {
    let region = Region::new(chunk_size);
    body(&region)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Counts, HEADER, Region, scope};
    use core::cell::Cell;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::vec::Vec;

    /// The tests' global allocator counts, for each thread (each test runs on its own),
    /// the blocks and the bytes it has out.
    struct Counting;

    std::thread_local! {
        static OUT: Cell<(isize, isize)> = const { Cell::new((0, 0)) };
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let block = unsafe { System.alloc(layout) };
            if !block.is_null() {
                let _ = OUT.try_with(|out| out.set((out.get().0 + 1, out.get().1 + layout.size() as isize)));
            }
            block
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            let _ = OUT.try_with(|out| out.set((out.get().0 - 1, out.get().1 - layout.size() as isize)));
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    fn out() -> (isize, isize) {
        OUT.with(|out| out.get())
    }

    #[test]
    fn a_region_takes_nothing_until_it_is_used() {
        let before = out();
        let region = Region::new(4096);
        assert_eq!(out(), before);
        assert_eq!(region.counts(), Counts::default());
        let n = region.alloc(1u8);
        assert_eq!(*n, 1);
        assert_eq!(out(), (before.0 + 1, before.1 + 4096));
    }

    #[test]
    fn objects_are_apart_aligned_and_keep_their_values() {
        let region = Region::new(4096);
        let a = region.alloc(0x11u8);
        let b = region.alloc(0x2222_2222_2222_2222u64);
        let c = region.alloc(0x33u8);
        let d = region.alloc([0x4444u16; 3]);
        let addr = |p: *const u8| p as usize;
        let (pa, pb, pc, pd) = (addr(a), addr(b as *const u64 as _), addr(c), addr(d as *const _ as _));
        assert_eq!(pb % 8, 0);
        assert_eq!(pd % 2, 0);
        assert_eq!((pb - pa, pc - pb, pd - pc), (8, 8, 2)); // 7 bytes of padding, then 1
        *a += 1;
        *b += 1;
        assert_eq!((*a, *b, *c, *d), (0x12, 0x2222_2222_2222_2223, 0x33, [0x4444; 3]));
        assert_eq!(region.counts().used, 8 + 8 + 2 + 6);
        assert_eq!(region.counts().blocks, 4);
    }

    #[test]
    fn strings_and_slices_are_copied_in() {
        let region = Region::new(4096);
        let original = std::string::String::from("the quick brown fox");
        let text = region.alloc_str(&original);
        drop(original);
        let words = region.alloc_slice(&[&text[0..3], &text[4..9]]);
        assert_eq!(words, ["the", "quick"]);
        text.make_ascii_uppercase();
        assert_eq!(text, "THE QUICK BROWN FOX");
    }

    #[test]
    fn a_full_chunk_is_followed_by_another() {
        let before = out();
        let region = Region::new(256);
        let per_chunk = (256 - HEADER) / 64;
        let blocks: Vec<&mut [u8; 64]> = (0..10).map(|i| region.alloc([i as u8; 64])).collect();
        let counts = region.counts();
        assert_eq!(counts.chunks, 10usize.div_ceil(per_chunk));
        assert_eq!((counts.held, counts.taken), (counts.chunks * 256, counts.chunks));
        assert_eq!(counts.used, 640);
        // Nothing moved or was overwritten when the later chunks were taken.
        assert!(blocks.iter().enumerate().all(|(i, block)| block.iter().all(|&byte| byte == i as u8)));
        assert_eq!(out().1 - before.1, (counts.held + blocks.capacity() * 8) as isize);
    }

    #[test]
    fn a_block_larger_than_a_chunk_gets_one_of_its_own() {
        let region = Region::new(256);
        let small = region.alloc(1u64);
        let large = region.alloc([7u8; 1000]);
        let after = region.alloc(2u64);
        assert_eq!((*small, large[999], *after), (1, 7, 2));
        let counts = region.counts();
        assert_eq!(counts.chunks, 2);
        assert!(counts.held >= 256 + 1000 + HEADER && counts.held < 256 + 1100);
        // A request too large for any allocator is an error, not a wrapped-around size.
        use core::alloc::Allocator;
        assert!(region.allocate(Layout::from_size_align(isize::MAX as usize - 64, 1).unwrap()).is_err());
    }

    #[test]
    fn a_block_can_ask_for_more_alignment_than_a_chunk_has() {
        #[repr(align(256))]
        #[derive(Clone, Copy)]
        struct Wide(u8);
        let region = Region::new(128);
        let a = region.alloc(Wide(1));
        let b = region.alloc(Wide(2));
        assert_eq!((a as *const Wide as usize % 256, b as *const Wide as usize % 256), (0, 0));
        assert_eq!((a.0, b.0), (1, 2));
    }

    #[test]
    fn dropping_the_region_gives_every_chunk_back() {
        let before = out();
        {
            let region = Region::new(256);
            for i in 0..100u64 {
                region.alloc([i; 8]);
            }
            assert_eq!(out().0 - before.0, region.counts().chunks as isize);
            assert!(region.counts().chunks > 25);
        }
        assert_eq!(out(), before);
    }

    #[test]
    fn a_reset_keeps_one_chunk_and_fills_it_again() {
        let before = out();
        let mut region = Region::new(256);
        let first = region.alloc(1u64) as *const u64 as usize;
        for i in 0..100u64 {
            region.alloc([i; 8]);
        }
        region.reset();
        let counts = region.counts();
        assert_eq!((counts.chunks, counts.held, counts.used, counts.resets), (1, 256, 0, 1));
        assert_eq!(out(), (before.0 + 1, before.1 + 256));
        assert!(counts.peak >= 6400 && counts.most_chunks > 25);
        // From now on the region is one chunk, reused: the same address after every reset.
        let again = region.alloc(2u64) as *const u64 as usize;
        for _ in 0..10 {
            region.reset();
            assert_eq!(region.alloc(3u64) as *const u64 as usize, again);
        }
        assert_eq!(region.counts().taken, counts.taken);
        assert_ne!(first, 0);
        drop(region);
        assert_eq!(out(), before);
    }

    #[test]
    fn resetting_a_region_that_was_never_used_does_nothing() {
        let before = out();
        let mut region = Region::new(256);
        region.reset();
        assert_eq!(out(), before);
        assert_eq!(*region.alloc(5u32), 5);
    }

    #[test]
    fn a_vector_grows_in_place_while_it_is_the_last_block() {
        let region = Region::new(4096);
        let mut words = Vec::new_in(&region);
        for i in 0..100u64 {
            words.push(i);
        }
        // 4, 8, 16, 32, 64, 128 elements: one block, grown five times where it was.
        assert_eq!(region.counts().blocks, 1);
        assert_eq!(region.counts().used, 128 * 8);
        let between = region.alloc(0u8);
        words.extend(100..200);
        // No longer the last block: the vector moved, and its old block is still there.
        assert_eq!(region.counts().blocks, 3);
        assert_eq!(region.counts().used, 128 * 8 + 8 + 256 * 8);
        let words: &mut [u64] = words.leak();
        assert!(words.iter().copied().eq(0..200));
        assert_eq!(*between, 0);
    }

    #[test]
    fn a_vector_that_outgrows_its_chunk_moves_to_the_next() {
        let region = Region::new(256);
        let mut bytes = Vec::new_in(&region);
        for i in 0..1000u32 {
            bytes.push(i as u8);
        }
        assert!(bytes.iter().copied().eq((0..1000u32).map(|i| i as u8)));
        assert!(region.counts().chunks >= 2);
    }

    #[test]
    fn the_last_block_can_be_given_back() {
        let region = Region::new(4096);
        let first = Vec::<u8, _>::with_capacity_in(100, &region);
        let second = Vec::<u8, _>::with_capacity_in(50, &region);
        assert_eq!(region.counts().used, 150);
        drop(first); // not the last: stays
        assert_eq!(region.counts().used, 150);
        drop(second);
        assert_eq!(region.counts().used, 100);
        assert_eq!(region.counts().peak, 150);
    }

    #[test]
    fn an_object_can_point_into_a_region_that_outlives_its_own() {
        struct Var<'s> {
            name: &'s str,
            next: Cell<Option<&'s Var<'s>>>,
        }
        struct Command<'c, 's> {
            words: &'c [&'c str],
            found: Option<&'s Var<'s>>,
        }
        let session = Region::new(4096);
        let a = session.alloc(Var { name: session.alloc_str("a"), next: Cell::new(None) });
        let b = session.alloc(Var { name: session.alloc_str("b"), next: Cell::new(Some(a)) });
        for _ in 0..3 {
            let command = Region::new(4096);
            let line = command.alloc_str("echo $a");
            let words = command.alloc_slice(&[&line[..4], &line[5..]]);
            let c = command.alloc(Command { words, found: b.next.get() });
            assert_eq!((c.words[1], c.found.map(|var| var.name)), ("$a", Some("a")));
        }
        assert_eq!(b.name, "b");
    }

    #[test]
    fn a_scope_frees_its_region_when_it_returns() {
        let before = out();
        let total = scope(256, |region| {
            let numbers = region.alloc_slice(&[1u64, 2, 3]);
            assert_eq!(out().0, before.0 + 1);
            numbers.iter().sum::<u64>()
        });
        assert_eq!(total, 6);
        assert_eq!(out(), before);
    }

    #[test]
    fn nothing_is_allocated_for_a_type_of_no_size() {
        let before = out();
        let region = Region::new(256);
        let unit = region.alloc(());
        let empty = region.alloc_str("");
        assert_eq!((*unit, &*empty), ((), ""));
        assert_eq!(out(), before);
        assert_eq!(region.counts().blocks, 0);
    }
}
