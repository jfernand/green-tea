//! Safe memory reclamation: when a lock-free structure may free a node it has unlinked.
//!
//! A [`stack::Stack`] and a [`queue::Queue`] keep their values in nodes from the heap, and
//! change their shape with compare-and-swap, so no thread ever waits for another. The
//! price is that a thread can be reading a node at the moment another unlinks it. The
//! thread that unlinks a node therefore doesn't free it. It *retires* it: hands it to a
//! [`Reclaim`], which frees it when no thread can still hold its address.
//!
//! Four of them are here:
//!
//! - [`AtOnce`] frees a retired node on the spot. It is wrong, and is here to be seen
//!   going wrong.
//! - [`Leak`] never frees. It is safe, and its memory grows with every node retired.
//! - [`epoch::Local`] frees a node once every thread has started a new operation since
//!   the node was retired, which it learns from a shared count of *epochs*.
//! - [`hazard::Local`] frees a node when no thread's *hazard pointers* hold its address.
//!
//! [`rcu::Rcu`] is the third structure: one value, replaced whole by its writers and read
//! without waiting by everyone else.
//!
//! None knows what a thread is. A caller gives each thread, or each hart, a number below
//! [`SLOTS`] and takes a `Local` for that number from the shared [`epoch::Epochs`] or
//! [`hazard::Hazards`].

#![no_std]

extern crate alloc;

pub mod epoch;
pub mod hazard;
pub mod queue;
pub mod rcu;
pub mod stack;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};

/// The most threads that can share an [`epoch::Epochs`] or a [`hazard::Hazards`].
pub const SLOTS: usize = 8;

/// What one thread's `Reclaim` has done so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Nodes handed to `retire`.
    pub retired: usize,
    /// Of those, the nodes freed.
    pub freed: usize,
    /// The most nodes that were retired and not yet freed, at any one time.
    pub peak: usize,
    /// Operations begun (`enter`).
    pub operations: usize,
    /// Pointers read with `protect`.
    pub protected: usize,
    /// Reads of words that belong to other threads: their announcements, or their hazard
    /// pointers.
    pub looked: usize,
    /// Epochs this thread advanced, or scans of the hazard pointers it made.
    pub rounds: usize,
}

impl Counts {
    /// Nodes retired and not yet freed.
    pub fn waiting(&self) -> usize {
        self.retired - self.freed
    }

    fn retire(&mut self) {
        self.retired += 1;
        self.peak = self.peak.max(self.waiting());
    }
}

impl core::ops::AddAssign for Counts {
    fn add_assign(&mut self, other: Counts) {
        self.retired += other.retired;
        self.freed += other.freed;
        self.peak += other.peak;
        self.operations += other.operations;
        self.protected += other.protected;
        self.looked += other.looked;
        self.rounds += other.rounds;
    }
}

/// One thread's side of a reclamation scheme. A structure brackets each operation with
/// `enter` and `exit`, reads every shared pointer it is going to follow with `protect`,
/// and passes each node it unlinks to `retire`.
pub trait Reclaim {
    /// An operation begins.
    fn enter(&mut self);

    /// Read `source`, and return a pointer that is safe to follow until `exit`, or until
    /// `protect` is called again with the same `index`. `index` is 0 or 1: a structure
    /// may need two nodes at once.
    fn protect<T>(&mut self, index: usize, source: &AtomicPtr<T>) -> *mut T;

    /// The operation is over: nothing `protect` returned will be followed again.
    fn exit(&mut self);

    /// Free `node` once no thread can still be using it.
    ///
    /// # Safety
    /// `node` came from `Box::into_raw`, has been unlinked so that no thread starting an
    /// operation now can reach it, and is retired once.
    unsafe fn retire<T>(&mut self, node: *mut T);

    fn counts(&self) -> Counts;
}

/// A retired node: its address, and the function that frees it.
struct Retired {
    node: *mut u8,
    free: unsafe fn(*mut u8),
}

impl Retired {
    fn new<T>(node: *mut T) -> Retired {
        unsafe fn free<T>(node: *mut u8) {
            // SAFETY: `retire`'s caller says it came from `Box::into_raw`, as a `T`.
            drop(unsafe { Box::from_raw(node as *mut T) });
        }
        Retired { node: node as *mut u8, free: free::<T> }
    }

    /// # Safety
    /// No thread holds the node's address any more.
    unsafe fn free(self) {
        unsafe { (self.free)(self.node) }
    }
}

/// Nodes that were still waiting when the thread that retired them gave up its `Local`.
/// They are freed when the `Epochs` or `Hazards` they were left with is dropped.
///
/// It is a stack that is pushed to and never popped, and so needs no reclamation of its
/// own: its nodes are freed by whoever owns it, alone.
struct Orphans {
    head: AtomicPtr<Orphan>,
}

struct Orphan {
    nodes: Vec<Retired>,
    next: *mut Orphan,
}

impl Orphans {
    const fn new() -> Orphans {
        Orphans { head: AtomicPtr::new(core::ptr::null_mut()) }
    }

    fn leave(&self, nodes: Vec<Retired>) {
        if nodes.is_empty() {
            return;
        }
        let orphan = Box::into_raw(Box::new(Orphan { nodes, next: core::ptr::null_mut() }));
        let mut head = self.head.load(Ordering::SeqCst);
        loop {
            // SAFETY: the orphan is ours until the compare-and-swap succeeds.
            unsafe { (*orphan).next = head };
            match self.head.compare_exchange(head, orphan, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return,
                Err(seen) => head = seen,
            }
        }
    }

    /// Free every node left here, and return how many there were.
    fn free(&mut self) -> usize {
        let mut freed = 0;
        let mut orphan = core::mem::replace(self.head.get_mut(), core::ptr::null_mut());
        while !orphan.is_null() {
            // SAFETY: `leave` made it with `Box::into_raw`, and `&mut self` says nobody
            // else is here.
            let Orphan { nodes, next } = *unsafe { Box::from_raw(orphan) };
            freed += nodes.len();
            for node in nodes {
                // SAFETY: the owner of the `Epochs` or `Hazards` is dropping it, so no
                // `Local` is left to hold a node's address.
                unsafe { node.free() };
            }
            orphan = next;
        }
        freed
    }
}

/// Frees a node the moment it is retired. A thread that read the node's address before
/// it was unlinked goes on to read freed memory, or memory that has since been handed
/// out again.
#[derive(Default)]
pub struct AtOnce {
    counts: Counts,
}

impl Reclaim for AtOnce {
    fn enter(&mut self) {
        self.counts.operations += 1;
    }

    fn protect<T>(&mut self, _index: usize, source: &AtomicPtr<T>) -> *mut T {
        self.counts.protected += 1;
        source.load(Ordering::SeqCst)
    }

    fn exit(&mut self) {}

    unsafe fn retire<T>(&mut self, node: *mut T) {
        self.counts.retire();
        self.counts.freed += 1;
        // SAFETY: none. That is the point.
        unsafe { Retired::new(node).free() };
    }

    fn counts(&self) -> Counts {
        self.counts
    }
}

/// Never frees a node. Safe, and the cost is every node the structure ever unlinked.
#[derive(Default)]
pub struct Leak {
    counts: Counts,
}

impl Reclaim for Leak {
    fn enter(&mut self) {
        self.counts.operations += 1;
    }

    fn protect<T>(&mut self, _index: usize, source: &AtomicPtr<T>) -> *mut T {
        self.counts.protected += 1;
        source.load(Ordering::SeqCst)
    }

    fn exit(&mut self) {}

    unsafe fn retire<T>(&mut self, _node: *mut T) {
        self.counts.retire();
    }

    fn counts(&self) -> Counts {
        self.counts
    }
}
