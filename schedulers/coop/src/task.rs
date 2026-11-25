//! A port of green-tea-v2/src/task.rs to bare metal. A task is an asymmetric coroutine:
//! someone `resume`s it, it runs until it calls `yield_now` or finishes, and control
//! returns to whoever resumed it.
//!
//! What changed from v2:
//! - `CURRENT` is a [`Global`] instead of a `thread_local!`: there is one hart.
//! - The stack comes from green-tea-bare's pool instead of an mmap. It has a guard page
//!   below it, as in v2, but only boards with paging make touching it fault (see
//!   `green-tea-bare::stack`).
//! - Each task has an `id`, so the demo can tell which one is about to run.
//! - A task's record comes from a slab cache in `green-tea-bare::kmem` instead of a `Box`.
//!   The cache keeps freed records constructed, and a constructed record has its stack:
//!   a task that is created takes a stack from the pool only when the cache has to make
//!   a new slab of records.
//!
//! Nothing here knows about interrupts. green-tea-preempt calls `yield_now` from its timer
//! handler, and keeps interrupts off whenever a switch is under way.

use alloc::boxed::Box;
use core::ops::Range;
use core::ptr::NonNull;
use green_tea_arch::{Context, swap_context};
use green_tea_bare::event::Event;
use green_tea_bare::kmem::{Cache, KernelCache};
use green_tea_bare::platform::{Board, Platform};
use green_tea_bare::stack::{SLOTS, Stack};
use green_tea_bare::{Global, GlobalPtr};

/// The task running right now, or null when we're on the boot stack.
static CURRENT: Global<GlobalPtr<Inner>> = Global::new(GlobalPtr::null());

/// Ids handed out in creation order, starting at 0.
static NEXT_ID: Global<usize> = Global::new(0);

/// The tasks' records. What the constructor does once per record, and a free leaves in
/// place, is the stack.
static TASKS: KernelCache = KernelCache::new("task", size_of::<Inner>(), align_of::<Inner>(), Some(construct), Some(destroy));

/// A record in its constructed state: no closure, not running, and a stack of its own.
/// Fails if the pool has no stack left.
fn construct(record: *mut u8) -> bool {
    let Some(stack) = Stack::try_take() else { return false };
    let inner = Inner { context: Context::default(), caller: Context::default(), func: None, done: false, id: 0, wake_at: None, waits_for: None, stack };
    // SAFETY: the cache hands the constructor memory of the size and alignment of an `Inner`.
    unsafe { record.cast::<Inner>().write(inner) };
    true
}

/// Undo `construct`: dropping the record gives its stack back to the pool.
fn destroy(record: *mut u8) {
    // SAFETY: the cache destroys only what `construct` constructed, once.
    unsafe { record.cast::<Inner>().drop_in_place() };
}

/// Look at the cache of task records: its sizes and its counts.
pub fn cache<R>(f: impl FnOnce(&Cache) -> R) -> R {
    TASKS.with(f)
}

/// Destroy the records no task is using, which gives their stacks back to the pool and
/// their pages back to the kernel. Returns how many slabs of records that was.
pub fn reap() -> usize {
    TASKS.reap()
}

/// An owned handle to a task. Dropping it gives the task's record back to the cache,
/// with its stack.
pub struct Task {
    inner: NonNull<Inner>,
}

struct Inner {
    context: Context, // the task's registers while it's suspended
    caller: Context,  // whoever resumed it; yield_now and finishing switch back here
    func: Option<Box<dyn FnOnce()>>,
    done: bool,
    id: usize,
    /// Set by `sleep_until` just before the task yields: don't run it again before this.
    wake_at: Option<u64>,
    /// Set by `wait_event` just before the task yields: don't run it until this is set.
    waits_for: Option<&'static Event>,
    stack: Stack,
}

impl Task {
    pub fn new(func: impl FnOnce() + 'static) -> Task {
        let id = NEXT_ID.replace(NEXT_ID.get() + 1);
        let Some(record) = TASKS.allocate() else {
            panic!("out of tasks: no page for a slab of records, or no stack for each (the pool has {SLOTS})");
        };
        let inner = record as *mut Inner;
        // SAFETY: the cache's object is a constructed `Inner`, and ours until it is freed.
        unsafe {
            let top = (*inner).stack.top(); // Context::new rounds it down to 16 bytes
            (*inner).context = Context::new(top, task_entry, inner.cast());
            (*inner).func = Some(Box::new(func));
            (*inner).done = false;
            (*inner).id = id;
            (*inner).wake_at = None;
            (*inner).waits_for = None;
            Task { inner: NonNull::new_unchecked(inner) }
        }
    }

    pub fn id(&self) -> usize {
        unsafe { (*self.inner.as_ptr()).id }
    }

    pub fn is_done(&self) -> bool {
        unsafe { (*self.inner.as_ptr()).done }
    }

    /// If the task went to sleep when it last yielded, until when. Taking it clears it.
    pub fn take_wake_at(&mut self) -> Option<u64> {
        unsafe { (*self.inner.as_ptr()).wake_at.take() }
    }

    /// If the task is waiting for an event since it last yielded, which. Taking it clears it.
    pub fn take_waits_for(&mut self) -> Option<&'static Event> {
        unsafe { (*self.inner.as_ptr()).waits_for.take() }
    }

    /// Run the task until it yields or finishes.
    pub fn resume(&mut self) {
        let inner = self.inner.as_ptr();
        assert!(!self.is_done(), "resumed a task that has already finished");
        let previous = CURRENT.replace(GlobalPtr::new(inner));
        unsafe { swap_context(&raw mut (*inner).caller, &raw const (*inner).context) };
        CURRENT.set(previous);
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        let inner = self.inner.as_ptr();
        // SAFETY: the record came from the cache in `new`, and a Task is dropped once. The
        // closure of a task that never ran goes here; the stack stays with the record.
        let freed = unsafe {
            (*inner).func = None;
            TASKS.free(inner as usize)
        };
        assert!(freed.is_ok(), "the cache refused a task's record: {freed:?}");
    }
}

/// Whether a task is running, as opposed to the code on the boot stack (`kmain`, the
/// scheduler).
pub fn in_task() -> bool {
    !CURRENT.get().is_null()
}

/// Suspend the current task and switch back to whoever resumed it.
///
/// # Panics
/// If called outside a task.
pub fn yield_now() {
    let inner = CURRENT.get().as_ptr();
    assert!(!inner.is_null(), "yield_now called outside a task");
    unsafe { swap_context(&raw mut (*inner).context, &raw const (*inner).caller) };
}

/// The guard page under the current task's stack.
///
/// # Panics
/// If called outside a task.
pub fn current_guard() -> Range<usize> {
    let inner = CURRENT.get().as_ptr();
    assert!(!inner.is_null(), "current_guard called outside a task");
    unsafe { (*inner).stack.guard() }
}

/// Suspend the current task, and tell the scheduler not to resume it before `ticks()`
/// reaches `deadline`.
///
/// # Panics
/// If called outside a task.
pub fn sleep_until(deadline: u64) {
    let inner = CURRENT.get().as_ptr();
    assert!(!inner.is_null(), "sleep_until called outside a task");
    unsafe { (*inner).wake_at = Some(deadline) };
    yield_now();
}

/// Suspend the current task until `event` is set. Returns at once if it already is. The
/// event stays set: clearing it is the caller's.
///
/// # Panics
/// If called outside a task.
pub fn wait_event(event: &'static Event) {
    let inner = CURRENT.get().as_ptr();
    assert!(!inner.is_null(), "wait_event called outside a task");
    if !event.is_set() {
        unsafe { (*inner).waits_for = Some(event) };
        yield_now();
    }
}

/// `sleep_until`, `ms` milliseconds from now.
pub fn sleep_ms(ms: u64) {
    sleep_until(Board::ticks() + ms * Board::ticks_per_ms());
}

/// Where every task starts, on its own stack: run the closure, mark the task done, and
/// switch back for the last time.
extern "C" fn task_entry(arg: *mut ()) -> ! {
    let inner: *mut Inner = arg.cast();
    unsafe {
        if let Some(func) = (*inner).func.take() {
            func(); // a panic here goes to the panic handler, which halts
        }
        (*inner).done = true;
        swap_context(&raw mut (*inner).context, &raw const (*inner).caller);
    }
    unreachable!("a finished task was resumed")
}
