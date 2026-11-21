//! A slab cache: an allocator for objects of one type, which keeps them constructed.
//!
//! After Jeff Bonwick's slab allocator (USENIX Summer 1994), in its layout for small
//! objects. A *slab* is one page: buffers of one size from its start, and the slab's own
//! record in its last 40 bytes. A buffer is an object and, after it, a word that links
//! the slab's free buffers, so that the link never overwrites the object.
//!
//! The cache makes a slab when it has no free object: it takes a page, and runs the
//! *constructor* on every object in it. `allocate` hands out an object as the constructor
//! left it, and `free` expects it back in that state. A slab whose objects have all come
//! back is kept, constructed, until [`Cache::reap`], which runs the *destructor* on each
//! object and gives the page back.
//!
//! Successive slabs start their buffers at different offsets, the slab's *colour*, taken
//! from the bytes that are left over when a page is cut into buffers.
//!
//! Not here, and in Bonwick's: slabs of more than one page with their records kept
//! elsewhere, for objects larger than an eighth of a page; and a lock.

use crate::{PAGE, PageSource};

const WORD: usize = size_of::<usize>();

/// A slab's record, at the end of its page.
#[repr(C)]
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

/// The bytes of a page that a slab's record takes.
pub const HEADER: usize = size_of::<Slab>();

/// Brings an object into its constructed state, given its address. `false` if it
/// couldn't, in which case the object is as if never constructed.
pub type Constructor = fn(*mut u8) -> bool;
/// Undoes a constructor.
pub type Destructor = fn(*mut u8);

/// The cache's three lists of slabs, by how many of a slab's objects are allocated.
const FULL: usize = 0; // all of them
const PARTIAL: usize = 1; // some
const EMPTY: usize = 2; // none

/// Why a free was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeError {
    /// The address isn't where one of its slab's objects starts.
    NotAnObject,
    /// The object is free already.
    NotAllocated,
}

/// What a cache has done.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub allocations: usize,
    /// Allocations refused: no free object, and no page or no constructed object to be had.
    pub refused: usize,
    pub frees: usize,
    pub slabs_created: usize,
    pub slabs_destroyed: usize,
    /// Calls of the constructor that succeeded, and calls of the destructor.
    pub constructed: usize,
    pub destroyed: usize,
}

pub struct Cache {
    name: &'static str,
    /// The size of an object, and where in a buffer the link is: after the object.
    size: usize,
    link_at: usize,
    align: usize,
    /// The size of a buffer: the object and the link, rounded up to the alignment.
    buf: usize,
    /// Buffers in a slab.
    per_slab: usize,
    /// How many colours there are, and which the next slab gets.
    colours: usize,
    next_colour: usize,
    constructor: Option<Constructor>,
    destructor: Option<Destructor>,
    /// The first slab of each list, or 0, and how many slabs each list has.
    heads: [usize; 3],
    slabs: [usize; 3],
    /// Objects allocated.
    in_use: usize,
    stats: Stats,
}

impl Cache {
    /// A cache of objects of `size` bytes aligned to `align`, with no slabs yet.
    ///
    /// # Panics
    /// If `align` isn't a power of two, or an object and its link don't fit in a page
    /// beside the slab's record.
    pub const fn new(
        name: &'static str,
        size: usize,
        align: usize,
        constructor: Option<Constructor>,
        destructor: Option<Destructor>,
    ) -> Cache {
        assert!(align.is_power_of_two() && size > 0, "an object needs a size, and an alignment that is a power of two");
        let align = if align < WORD { WORD } else { align };
        let link_at = size.next_multiple_of(WORD);
        let buf = (link_at + WORD).next_multiple_of(align);
        let per_slab = (PAGE - HEADER) / buf;
        assert!(per_slab > 0, "an object too large for a slab of one page");
        let colours = (PAGE - HEADER - per_slab * buf) / align + 1;
        Cache {
            name,
            size,
            link_at,
            align,
            buf,
            per_slab,
            colours,
            next_colour: 0,
            constructor,
            destructor,
            heads: [0; 3],
            slabs: [0; 3],
            in_use: 0,
            stats: Stats {
                allocations: 0,
                refused: 0,
                frees: 0,
                slabs_created: 0,
                slabs_destroyed: 0,
                constructed: 0,
                destroyed: 0,
            },
        }
    }

    /// The address of an object in its constructed state. `None` if the cache has no free
    /// object and can't make a slab: `pages` has no page, or a constructor failed.
    #[inline(never)]
    pub fn allocate(&mut self, pages: &mut impl PageSource) -> Option<usize> {
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

    /// Take an object back. It must be in its constructed state.
    ///
    /// # Safety
    /// `object` must be an address that `allocate` on this cache returned. The cache
    /// refuses an address that isn't the start of an object of the slab it is in, and an
    /// object that is free already; it can't tell an address from another cache's slab,
    /// or one that was never in a slab.
    #[inline(never)]
    pub unsafe fn free(&mut self, object: usize) -> Result<(), FreeError> {
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
        self.in_use -= 1;
        self.stats.frees += 1;
        Ok(())
    }

    /// Give back every slab that has no object allocated: run the destructor on each of
    /// its objects, and return its page to `pages`. Returns how many slabs that was.
    pub fn reap(&mut self, pages: &mut impl PageSource) -> usize {
        let mut reaped = 0;
        while self.heads[EMPTY] != 0 {
            let slab = self.heads[EMPTY];
            self.unlink(slab, EMPTY);
            // SAFETY: the slab was on the cache's list, and none of its objects is in use.
            unsafe {
                let first = (*(slab as *const Slab)).first;
                if let Some(destructor) = self.destructor {
                    for index in 0..self.per_slab {
                        destructor((first + index * self.buf) as *mut u8);
                        self.stats.destroyed += 1;
                    }
                }
                pages.give_page(slab + HEADER - PAGE);
            }
            self.stats.slabs_destroyed += 1;
            reaped += 1;
        }
        reaped
    }

    /// Make a slab: take a page, construct every object in it, and put the slab on the
    /// list of empty ones. Returns the slab's record.
    #[cold]
    fn grow(&mut self, pages: &mut impl PageSource) -> Option<usize> {
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

    /// The list a slab with `in_use` objects allocated belongs on.
    fn list_for(&self, in_use: usize) -> usize {
        if in_use == 0 {
            EMPTY
        } else if in_use == self.per_slab {
            FULL
        } else {
            PARTIAL
        }
    }

    /// Move a slab to another list, if a change in its count of allocated objects calls
    /// for it.
    fn moved(&mut self, slab: usize, was: usize, now: usize) {
        let (from, to) = (self.list_for(was), self.list_for(now));
        if from != to {
            self.unlink(slab, from);
            self.push(slab, to);
        }
    }

    fn push(&mut self, slab: usize, list: usize) {
        let head = self.heads[list];
        // SAFETY: `slab` and the list's head are slabs' records.
        unsafe {
            (*(slab as *mut Slab)).next = head;
            (*(slab as *mut Slab)).prev = 0;
            if head != 0 {
                (*(head as *mut Slab)).prev = slab;
            }
        }
        self.heads[list] = slab;
        self.slabs[list] += 1;
    }

    fn unlink(&mut self, slab: usize, list: usize) {
        // SAFETY: `slab` is on `list`, so it and its neighbours are slabs' records.
        unsafe {
            let (next, prev) = ((*(slab as *const Slab)).next, (*(slab as *const Slab)).prev);
            match prev {
                0 => self.heads[list] = next,
                prev => (*(prev as *mut Slab)).next = next,
            }
            if next != 0 {
                (*(next as *mut Slab)).prev = prev;
            }
        }
        self.slabs[list] -= 1;
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The size of an object.
    pub fn object_size(&self) -> usize {
        self.size
    }

    /// The size of a buffer: an object, the link after it, and padding to the alignment.
    pub fn buffer_size(&self) -> usize {
        self.buf
    }

    /// How many objects a slab holds.
    pub fn per_slab(&self) -> usize {
        self.per_slab
    }

    /// The bytes of a page that are neither buffers nor the slab's record.
    pub fn spare(&self) -> usize {
        PAGE - HEADER - self.per_slab * self.buf
    }

    /// How many different offsets a slab's first buffer can have.
    pub fn colours(&self) -> usize {
        self.colours
    }

    /// How many slabs have every object allocated, some, and none.
    pub fn slabs(&self) -> (usize, usize, usize) {
        (self.slabs[FULL], self.slabs[PARTIAL], self.slabs[EMPTY])
    }

    /// The pages the cache holds: one for each slab.
    pub fn pages(&self) -> usize {
        self.slabs.iter().sum()
    }

    /// Objects allocated.
    pub fn in_use(&self) -> usize {
        self.in_use
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Walk the lists and every slab's free list, and check that they agree with the
    /// counts.
    pub fn check(&self) -> Result<(), &'static str> {
        let mut in_use = 0;
        for list in [FULL, PARTIAL, EMPTY] {
            let (mut slab, mut prev, mut count) = (self.heads[list], 0, 0);
            while slab != 0 {
                // SAFETY: on one of the cache's lists, so a slab's record.
                let header = unsafe { &*(slab as *const Slab) };
                if header.prev != prev || self.list_for(header.in_use) != list || header.in_use > self.per_slab {
                    return Err("a slab is on the wrong list");
                }
                let (mut free, mut length) = (header.free, 0);
                while free != 0 {
                    let offset = free.wrapping_sub(header.first);
                    if !offset.is_multiple_of(self.buf) || offset / self.buf >= self.per_slab || length == self.per_slab
                    {
                        return Err("a slab's free list leaves the slab");
                    }
                    length += 1;
                    // SAFETY: checked above to be one of the slab's buffers.
                    free = unsafe { ((free + self.link_at) as *const usize).read() };
                }
                if length + header.in_use != self.per_slab {
                    return Err("a slab's free list doesn't hold its free objects");
                }
                in_use += header.in_use;
                count += 1;
                if count > self.slabs[list] {
                    return Err("a list of slabs is longer than its count");
                }
                (prev, slab) = (slab, header.next);
            }
            if count != self.slabs[list] {
                return Err("a slab is on no list");
            }
        }
        if in_use != self.in_use {
            return Err("the slabs' counts don't add up to the cache's");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Cache, FreeError, HEADER};
    use crate::frames::{Frame, Frames};
    use crate::{PAGE, PageSource};
    use std::alloc::{Layout, alloc, dealloc};
    use std::cell::Cell;
    use std::vec;
    use std::vec::Vec;

    /// Pages from the host's allocator, up to a limit, and a count of the ones out.
    struct HostPages {
        left: usize,
        out: Vec<usize>,
    }

    const LAYOUT: Layout = Layout::new::<Page>();
    #[repr(align(4096))]
    struct Page(#[allow(dead_code)] [u8; PAGE]);

    impl HostPages {
        fn new(limit: usize) -> HostPages {
            HostPages { left: limit, out: Vec::new() }
        }
    }

    impl PageSource for HostPages {
        fn take_page(&mut self) -> Option<usize> {
            self.left = self.left.checked_sub(1)?;
            let page = unsafe { alloc(LAYOUT) } as usize;
            self.out.push(page);
            Some(page)
        }

        unsafe fn give_page(&mut self, page: usize) {
            let at = self.out.iter().position(|out| *out == page).expect("a page given back that wasn't taken");
            self.out.swap_remove(at);
            self.left += 1;
            unsafe { dealloc(page as *mut u8, LAYOUT) };
        }
    }

    impl Drop for HostPages {
        fn drop(&mut self) {
            for page in self.out.drain(..) {
                unsafe { dealloc(page as *mut u8, LAYOUT) };
            }
        }
    }

    // What the test objects' constructor and destructor have done, on this test's thread.
    std::thread_local! {
        static CONSTRUCTED: Cell<usize> = const { Cell::new(0) };
        static DESTROYED: Cell<usize> = const { Cell::new(0) };
        static FAIL_AFTER: Cell<usize> = const { Cell::new(usize::MAX) };
    }

    /// The constructed state of a test object: a mark in its first word and in its last.
    const MARK: usize = 0x636f_6e73_7472_7563;
    const SIZE: usize = 200;

    fn construct(object: *mut u8) -> bool {
        if CONSTRUCTED.get() >= FAIL_AFTER.get() {
            return false;
        }
        CONSTRUCTED.set(CONSTRUCTED.get() + 1);
        unsafe {
            (object as *mut usize).write(MARK);
            (object.add(SIZE - 8) as *mut usize).write(MARK);
        }
        true
    }

    fn destroy(object: *mut u8) {
        assert!(constructed(object as usize), "an object destroyed that wasn't in its constructed state");
        DESTROYED.set(DESTROYED.get() + 1);
        unsafe { (object as *mut usize).write(0) };
    }

    fn constructed(object: usize) -> bool {
        unsafe { (object as *const usize).read() == MARK && ((object + SIZE - 8) as *const usize).read() == MARK }
    }

    fn cache() -> Cache {
        Cache::new("test", SIZE, 8, Some(construct), Some(destroy))
    }

    #[test]
    fn a_page_is_cut_into_buffers_with_the_record_at_its_end() {
        let cache = cache();
        // 200 bytes and a link of 8: 19 buffers of 208 are 3,952 bytes, the record is 40,
        // and 104 are left over, which is 14 colours 8 bytes apart.
        assert_eq!(
            (cache.buffer_size(), cache.per_slab(), HEADER, cache.spare(), cache.colours()),
            (208, 19, 40, 104, 14)
        );
        let aligned = Cache::new("aligned", 100, 64, None, None);
        assert_eq!((aligned.buffer_size(), aligned.per_slab(), aligned.colours()), (128, 31, 2));
        let one = Cache::new("one", PAGE - HEADER - 8, 8, None, None);
        assert_eq!((one.per_slab(), one.colours()), (1, 1));
    }

    #[test]
    fn a_slab_is_constructed_once_and_handed_out_in_address_order() {
        let (mut cache, mut pages) = (cache(), HostPages::new(4));
        let first = cache.allocate(&mut pages).unwrap();
        assert_eq!(first % PAGE, 0, "the first slab has colour 0");
        assert_eq!((CONSTRUCTED.get(), cache.stats().constructed, cache.slabs()), (19, 19, (0, 1, 0)));
        let rest: Vec<usize> = (1..19).map(|_| cache.allocate(&mut pages).unwrap()).collect();
        assert!(rest.iter().enumerate().all(|(i, object)| *object == first + (i + 1) * 208));
        assert!(rest.iter().all(|object| constructed(*object)));
        assert_eq!((cache.slabs(), cache.in_use(), pages.out.len()), ((1, 0, 0), 19, 1));
        // The twentieth object needs a second slab, whose buffers start 8 bytes in.
        let twentieth = cache.allocate(&mut pages).unwrap();
        assert_eq!(twentieth % PAGE, 8);
        assert_eq!((CONSTRUCTED.get(), cache.slabs(), pages.out.len()), (38, (1, 1, 0), 2));
        cache.check().unwrap();
    }

    #[test]
    fn freed_objects_come_back_without_being_constructed_again() {
        let (mut cache, mut pages) = (cache(), HostPages::new(4));
        let objects: Vec<usize> = (0..30).map(|_| cache.allocate(&mut pages).unwrap()).collect();
        for object in &objects {
            unsafe { cache.free(*object).unwrap() };
        }
        // Both slabs are kept, and so is what the constructor did.
        assert_eq!((cache.slabs(), cache.in_use(), pages.out.len(), DESTROYED.get()), ((0, 0, 2), 0, 2, 0));
        assert!(objects.iter().all(|object| constructed(*object)), "a free overwrote constructed state");
        let again: Vec<usize> = (0..30).map(|_| cache.allocate(&mut pages).unwrap()).collect();
        assert_eq!(CONSTRUCTED.get(), 38, "objects were constructed a second time");
        assert_eq!(cache.stats().slabs_created, 2);
        // They are objects of the same two slabs, though not the same thirty of them.
        let slabs: Vec<usize> = objects.iter().map(|object| object / PAGE).collect();
        assert!(again.iter().all(|object| slabs.contains(&(object / PAGE)) && constructed(*object)));
        assert_eq!(pages.out.len(), 2);
        cache.check().unwrap();
    }

    #[test]
    fn the_object_freed_last_is_handed_out_next() {
        let (mut cache, mut pages) = (cache(), HostPages::new(1));
        let objects: Vec<usize> = (0..19).map(|_| cache.allocate(&mut pages).unwrap()).collect();
        for index in [3, 11, 7] {
            unsafe { cache.free(objects[index]).unwrap() };
        }
        let next: Vec<usize> = (0..3).map(|_| cache.allocate(&mut pages).unwrap()).collect();
        assert_eq!(next, [objects[7], objects[11], objects[3]]);
        assert_eq!(cache.allocate(&mut pages), None, "the only page is full");
        assert_eq!(cache.stats().refused, 1);
    }

    #[test]
    fn reaping_destroys_the_slabs_nobody_uses_and_only_those() {
        let (mut cache, mut pages) = (cache(), HostPages::new(8));
        let objects: Vec<usize> = (0..57).map(|_| cache.allocate(&mut pages).unwrap()).collect();
        assert_eq!(cache.slabs(), (3, 0, 0));
        // Free the second slab's objects, and all but one of the third's.
        for object in &objects[19..56] {
            unsafe { cache.free(*object).unwrap() };
        }
        assert_eq!(cache.slabs(), (1, 1, 1));
        assert_eq!(cache.reap(&mut pages), 1);
        assert_eq!((cache.slabs(), pages.out.len(), DESTROYED.get()), ((1, 1, 0), 2, 19));
        assert_eq!(cache.reap(&mut pages), 0);
        // The one object left in the third slab is what keeps its page.
        unsafe { cache.free(objects[56]).unwrap() };
        assert_eq!(cache.reap(&mut pages), 1);
        assert_eq!((cache.pages(), pages.out.len(), DESTROYED.get()), (1, 1, 38));
        let stats = cache.stats();
        assert_eq!((stats.slabs_created, stats.slabs_destroyed, stats.constructed, stats.destroyed), (3, 2, 57, 38));
        cache.check().unwrap();
    }

    #[test]
    fn a_partly_used_slab_is_filled_before_an_empty_one_is_touched() {
        let (mut cache, mut pages) = (cache(), HostPages::new(8));
        let objects: Vec<usize> = (0..38).map(|_| cache.allocate(&mut pages).unwrap()).collect();
        for object in &objects[..19] {
            unsafe { cache.free(*object).unwrap() }; // the first slab, now empty
        }
        unsafe { cache.free(objects[30]).unwrap() }; // one of the second's
        assert_eq!(cache.slabs(), (0, 1, 1));
        assert_eq!(cache.allocate(&mut pages), Some(objects[30]));
        assert_eq!(cache.slabs(), (1, 0, 1));
    }

    #[test]
    fn successive_slabs_start_at_different_offsets() {
        let (mut cache, mut pages) = (cache(), HostPages::new(16));
        let mut offsets = Vec::new();
        for _ in 0..16 {
            let first = cache.allocate(&mut pages).unwrap();
            offsets.push(first % PAGE);
            (1..19).for_each(|_| assert!(cache.allocate(&mut pages).is_some()));
        }
        // Fourteen colours, 0 to 104, and then round again.
        assert_eq!(offsets, [0, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 96, 104, 0, 8]);
        // With the largest colour, the last buffer ends where the slab's record starts.
        assert_eq!(104 + 19 * 208, PAGE - HEADER);
        cache.check().unwrap();
    }

    #[test]
    fn mistaken_frees_are_refused() {
        let (mut cache, mut pages) = (cache(), HostPages::new(2));
        let a = cache.allocate(&mut pages).unwrap();
        let b = cache.allocate(&mut pages).unwrap();
        unsafe {
            assert_eq!(cache.free(a + 8), Err(FreeError::NotAnObject), "inside an object");
            assert_eq!(cache.free(a + 19 * 208), Err(FreeError::NotAnObject), "past the last buffer");
            assert_eq!(cache.free(a), Ok(()));
            assert_eq!(cache.free(a), Err(FreeError::NotAllocated), "a second free");
            assert_eq!(cache.free(b + 208), Err(FreeError::NotAllocated), "never allocated");
        }
        assert_eq!((cache.in_use(), cache.stats().frees), (1, 1));
        cache.check().unwrap();
    }

    #[test]
    fn a_cache_with_no_page_to_be_had_refuses() {
        let (mut cache, mut pages) = (cache(), HostPages::new(0));
        assert_eq!(cache.allocate(&mut pages), None);
        assert_eq!((cache.pages(), cache.stats().refused, CONSTRUCTED.get()), (0, 1, 0));
    }

    #[test]
    fn a_constructor_that_fails_undoes_its_slab() {
        let (mut cache, mut pages) = (cache(), HostPages::new(4));
        FAIL_AFTER.set(19 + 5); // the second slab's sixth object
        let objects: Vec<usize> = (0..19).map(|_| cache.allocate(&mut pages).unwrap()).collect();
        assert_eq!(cache.allocate(&mut pages), None);
        assert_eq!((cache.slabs(), pages.out.len()), ((1, 0, 0), 1), "the page of the slab that failed was kept");
        assert_eq!((CONSTRUCTED.get(), DESTROYED.get()), (24, 5));
        assert_eq!((cache.stats().constructed, cache.stats().destroyed), (24, 5), "the failed slab's calls");
        // The cache still works: a freed object is handed out, and a slab can be made
        // once the constructor succeeds.
        unsafe { cache.free(objects[4]).unwrap() };
        assert_eq!(cache.allocate(&mut pages), Some(objects[4]));
        FAIL_AFTER.set(usize::MAX);
        assert!(cache.allocate(&mut pages).is_some());
        cache.check().unwrap();
    }

    #[test]
    fn a_cache_over_page_frames_gives_every_page_back() {
        // Real memory for 64 frames, and the frame allocator over it.
        let layout = Layout::from_size_align(64 * PAGE, PAGE).unwrap();
        let base = unsafe { alloc(layout) } as usize;
        let mut table = vec![Frame::NEW; 64];
        let mut frames = unsafe { Frames::new(base, &mut table, 0) };
        let before = frames.free_blocks();
        let mut cache = cache();
        let objects: Vec<usize> = (0..19 * 64).map(|_| cache.allocate(&mut frames).unwrap()).collect();
        assert_eq!(cache.allocate(&mut frames), None, "every frame is a slab");
        assert_eq!((frames.free_pages(), cache.pages()), (0, 64));
        for object in objects.iter().rev() {
            unsafe { cache.free(*object).unwrap() };
        }
        assert_eq!(frames.free_pages(), 0, "empty slabs are kept until they are reaped");
        assert_eq!(cache.reap(&mut frames), 64);
        assert_eq!(frames.free_blocks(), before);
        assert_eq!((CONSTRUCTED.get(), DESTROYED.get()), (19 * 64, 19 * 64));
        frames.check().unwrap();
        unsafe { dealloc(base as *mut u8, layout) };
    }
}
