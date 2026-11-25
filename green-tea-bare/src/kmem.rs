//! What the kernel allocates for itself with, under the heap: page frames, and slab caches
//! of objects cut from them. The allocators are green-tea-kmem's; here is the kernel's one
//! instance of each.
//!
//! The page frames are the [`REGION`] bytes of RAM after the kernel's last section.
//! `cargo xtask kernel` starts QEMU with 128 MiB, and the kernel and this region end well
//! inside that on every board. On a machine with too little, the first use panics. The
//! allocator's table, a record per frame, is in the region's first pages, which are never
//! handed out.
//!
//! A [`KernelCache`] is a slab cache in a static, whose slabs are pages from here. Every
//! operation on the frames or on a cache runs with interrupts off, as the heap's do, and
//! none may be made from inside another: a constructor that allocated from a cache would
//! panic.

use crate::global::Global;
use crate::platform::{Board, Platform, TimerInterrupt};
use core::cell::UnsafeCell;
use core::hint::black_box;
use core::ops::Range;
use core::slice;

pub use green_tea_kmem::frames::{self, Frame, Frames};
pub use green_tea_kmem::slab::{self, Cache, Constructor, Destructor};
pub use green_tea_kmem::{PAGE, PageSource};

unsafe extern "C" {
    static __kernel_end: u8;
}

/// How much RAM the page allocator manages, from the kernel's end up.
pub const REGION: usize = 16 * 1024 * 1024;
/// How many page frames that is.
pub const PAGES: usize = REGION / PAGE;
/// The pages at the start of the region that hold the table of frames.
pub const TABLE_PAGES: usize = (PAGES * size_of::<Frame>()).div_ceil(PAGE);

/// The region's addresses.
pub fn region() -> Range<usize> {
    // Through `black_box` for the reason in memory.rs: the symbol's address is another's.
    let start = black_box(&raw const __kernel_end as usize);
    start..start + REGION
}

struct Kmem(UnsafeCell<Option<Frames<'static>>>); // None until the first use

// SAFETY: one hart, and `locked` keeps interrupts off while anything touches the frames.
unsafe impl Sync for Kmem {}

static KMEM: Kmem = Kmem(UnsafeCell::new(None));

/// Set while `locked` is running.
static BUSY: Global<bool> = Global::new(false);

/// Make the allocator, on its first use: the table at the start of the region, and
/// everything after the table free.
#[cold]
fn start() -> Frames<'static> {
    let Range { start: base, end } = region();
    if let Some(ram_end) = Board::ram_end() {
        assert!(end <= ram_end, "the page frames end at {end:#x}, past the end of RAM at {ram_end:#x}");
    }
    let table = base as *mut Frame;
    // SAFETY: linker.ld ends the kernel on a page, the RAM after it is there (checked above,
    // or a fault if not) and nothing else uses it. The table is written before a reference to it is made.
    unsafe {
        (0..PAGES).for_each(|frame| table.add(frame).write(Frame::NEW));
        Frames::new(base, slice::from_raw_parts_mut(table, PAGES), TABLE_PAGES)
    }
}

fn locked<R>(f: impl FnOnce(&mut Frames<'static>) -> R) -> R {
    Board::without_interrupts(|| {
        assert!(!BUSY.replace(true), "the page allocator was used from inside itself");
        // SAFETY: interrupts are off and BUSY was clear, so this is the only reference.
        let frames = unsafe { &mut *KMEM.0.get() }.get_or_insert_with(start);
        let result = f(frames);
        BUSY.set(false);
        result
    })
}

/// Run `f` on the kernel's page frames, with interrupts off. `f` must not use the frames
/// or a [`KernelCache`] itself.
pub fn with_frames<R>(f: impl FnOnce(&mut Frames<'static>) -> R) -> R {
    locked(f)
}

/// A slab cache that can be a `static`, and takes its pages from the kernel's frames.
pub struct KernelCache(UnsafeCell<Cache>);

// SAFETY: as for `Kmem`: every use is inside `locked`.
unsafe impl Sync for KernelCache {}

impl KernelCache {
    /// A cache of objects of `size` bytes aligned to `align`. See [`Cache::new`].
    pub const fn new(
        name: &'static str,
        size: usize,
        align: usize,
        constructor: Option<Constructor>,
        destructor: Option<Destructor>,
    ) -> KernelCache {
        KernelCache(UnsafeCell::new(Cache::new(name, size, align, constructor, destructor)))
    }

    /// An object in its constructed state, or `None` if there is none to be had.
    pub fn allocate(&self) -> Option<usize> {
        // SAFETY: inside `locked`, so this is the only reference to the cache.
        locked(|frames| {
            // SAFETY: exclusive under `locked`; the kernel page allocator outlives this cache.
            unsafe { (&mut *self.0.get()).allocate(frames) }
        })
    }

    /// Take an object back, in its constructed state.
    ///
    /// # Safety
    /// `object` must have come from `allocate` on this cache. See [`Cache::free`].
    pub unsafe fn free(&self, object: usize) -> Result<(), slab::FreeError> {
        // SAFETY: as for `allocate`; the caller's promise is ours to the cache.
        locked(|_| unsafe { (*self.0.get()).free(object) })
    }

    /// Destroy the slabs that have no object allocated, and give their pages back.
    /// Returns how many there were.
    pub fn reap(&self) -> usize {
        // SAFETY: as for `allocate`.
        locked(|frames| {
            // SAFETY: exclusive under `locked`; the kernel page allocator outlives this cache.
            unsafe { (&mut *self.0.get()).reap(frames) }
        })
    }

    /// Look at the cache: its sizes, its counts.
    pub fn with<R>(&self, f: impl FnOnce(&Cache) -> R) -> R {
        // SAFETY: as for `allocate`.
        locked(|_| f(unsafe { &*self.0.get() }))
    }
}
