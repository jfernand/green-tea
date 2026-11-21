//! The kernel's heap: a [`Tlsf`] over the region linker.ld reserves. It is the global
//! allocator, so it is what `Box`, `Vec` and the rest of the `alloc` crate allocate from.
//!
//! Blocks are of any size, and a freed block is merged with its free neighbours and
//! reused. The free blocks are on lists by size, and two bitmaps say which lists have a
//! block, so neither an allocation nor a free searches for anything: what each does has
//! a bound that doesn't depend on the state of the heap. That matters here because of the
//! next paragraph. green-tea-fit has the allocator, and the ones that search.
//!
//! Every operation runs with interrupts off, so that a timer interrupt can't switch to
//! another task half way through one (green-tea-preempt), and holding a spin lock, so
//! that another CPU can't be in the allocator at the same time ([`crate::cpus`]). The
//! lock remembers which CPU holds it, so that a use of the heap from inside itself is a
//! panic and not a wait that never ends.
//!
//! The heap can record what is asked of it: [`record`] takes a buffer, and from then on
//! every allocation and every free is written to it as a green-tea-fit trace event, until
//! [`stop_recording`] hands the buffer back.

use crate::platform::{Board, Platform, TimerInterrupt};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use alloc::boxed::Box;
use core::ptr;
use green_tea_fit::trace::Event;
use green_tea_fit::tlsf::Tlsf;

unsafe extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

/// The allocator the heap is, for the tests to print.
pub const NAME: &str = "TLSF";

/// `addr` rounded up to a multiple of `align`, which must be a power of two, as a
/// `Layout`'s alignment always is. `None` if that overflows.
///
/// `checked_next_multiple_of` does the same for any `align`, with a division: the compiler
/// can't see that a `Layout`'s alignment is a power of two. This is an add and a mask.
pub fn align_up(addr: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    Some(addr.checked_add(align - 1)? & !(align - 1))
}

struct Heap {
    fit: UnsafeCell<Option<Tlsf>>, // None until the first use
    recording: UnsafeCell<Option<Recording>>,
}

/// A trace being recorded: the buffer, how many events are in it, and how many didn't fit.
pub struct Recording {
    pub events: Box<[Event]>,
    pub len: usize,
    pub dropped: usize,
}

impl Recording {
    fn push(&mut self, event: Event) {
        match self.events.get_mut(self.len) {
            Some(slot) => (*slot, self.len) = (event, self.len + 1),
            None => self.dropped += 1,
        }
    }
}

// SAFETY: `locked` holds the lock, with interrupts off, while anything touches either field.
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap { fit: UnsafeCell::new(None), recording: UnsafeCell::new(None) };

/// The heap's lock: 0 when nobody is in the heap, or else the number of the CPU that is,
/// plus one.
static OWNER: AtomicUsize = AtomicUsize::new(0);

/// How many times the lock has been taken, and how many of those found it held by
/// another CPU and waited.
static TAKEN: AtomicUsize = AtomicUsize::new(0);
static WAITED: AtomicUsize = AtomicUsize::new(0);

/// How many times the heap's lock has been taken, and how many of those had to wait for
/// another CPU. Reading them doesn't take the lock.
pub fn lock_counts() -> (usize, usize) {
    (TAKEN.load(Ordering::Relaxed), WAITED.load(Ordering::Relaxed))
}

/// Run `f` on the heap's allocator, with interrupts off. `f` must not allocate or free:
/// that would be a second use of the allocator inside the first, and panics.
pub fn with<R>(f: impl FnOnce(&mut Tlsf) -> R) -> R {
    locked(|fit, _| f(fit))
}

/// Record every allocation and free into `events`, from the next one on, until
/// [`stop_recording`]. Returns the recording this one replaces, if there was one.
pub fn record(events: Box<[Event]>) -> Option<Recording> {
    locked(|_, recording| recording.replace(Recording { events, len: 0, dropped: 0 }))
}

/// Stop recording, and return what was recorded.
pub fn stop_recording() -> Option<Recording> {
    locked(|_, recording| recording.take())
}

/// Whether a recording is under way.
pub fn is_recording() -> bool {
    locked(|_, recording| recording.is_some())
}

/// Make the allocator, on the heap's first use. Out of line on purpose: a `Tlsf` is 3 KiB
/// of table, and built in `locked` it would be in the stack frame of every allocation,
/// on task stacks of 16 KiB.
#[cold]
#[inline(never)]
fn start(slot: &mut Option<Tlsf>) {
    let start = &raw const __heap_start as usize;
    let end = &raw const __heap_end as usize;
    // SAFETY: linker.ld reserves the region for the heap, and nothing else uses it.
    *slot = Some(unsafe { Tlsf::new(start, end - start) });
}

fn locked<R>(f: impl FnOnce(&mut Tlsf, &mut Option<Recording>) -> R) -> R {
    Board::without_interrupts(|| {
        let me = Board::cpu() + 1;
        let mut waited = false;
        // Write our number if the lock holds 0, in one step no other CPU can come between.
        while let Err(owner) = OWNER.compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed) {
            assert!(owner != me, "the heap was used from inside heap::with");
            waited = true;
            spin_loop();
        }
        // We hold the lock, so nobody else is writing the count.
        TAKEN.store(TAKEN.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        if waited {
            WAITED.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: interrupts are off and the lock is ours, so this is the only reference.
        let slot = unsafe { &mut *HEAP.fit.get() };
        if slot.is_none() {
            start(slot);
        }
        let fit = slot.as_mut().expect("the heap was started above");
        // SAFETY: as for `fit`.
        let result = f(fit, unsafe { &mut *HEAP.recording.get() });
        OWNER.store(0, Ordering::Release);
        result
    })
}

/// Bytes of the heap in allocated blocks, each block's two tags included.
pub fn used() -> usize {
    with(|fit| fit.used())
}

/// The heap's size: what linker.ld reserves, less a word at each end.
pub fn capacity() -> usize {
    with(|fit| fit.capacity())
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Null if nothing fits: the alloc crate turns that into an allocation-error panic.
        let allocated = locked(|fit, recording| {
            let addr = fit.allocate(layout.size(), layout.align())?;
            if let Some(recording) = recording {
                recording.push(Event::allocate(addr, layout.size(), layout.align()));
            }
            Some(addr)
        });
        allocated.map_or(ptr::null_mut(), |addr| addr as *mut u8)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: the caller's promise is ours to the allocator.
        let freed = locked(|fit, recording| {
            if let Some(recording) = recording {
                recording.push(Event::free(ptr as usize));
            }
            unsafe { fit.free(ptr as usize) }
        });
        if let Err(e) = freed {
            panic!("free of {ptr:p}: {e:?}");
        }
    }
}
