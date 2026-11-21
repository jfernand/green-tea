//! A lock-free queue, after Maged Michael and Michael Scott: a linked list with a pointer
//! to each end, and a node at the front that holds no value.
//!
//! That first node, the *dummy*, means the list is never empty, so `enqueue` and
//! `dequeue` never work on the same pointer. `enqueue` links a node after the last one
//! and then swings `tail` to it, in two compare-and-swaps; a thread that finds `tail` one
//! node behind moves it forward before doing anything else, so nobody waits for the
//! thread that fell between the two. `dequeue` swings `head` to the second node, whose
//! value it takes: that node is the new dummy, and the old one is retired.
//!
//! `dequeue` follows two pointers, `head` and the first node's `next`, and so asks its
//! [`Reclaim`] to protect two nodes at once.

use crate::Reclaim;
use alloc::boxed::Box;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

struct Node<T> {
    next: AtomicPtr<Node<T>>,
    /// Written by `enqueue`, and moved out by the `dequeue` that makes this node the
    /// dummy. The first dummy has none.
    value: MaybeUninit<T>,
}

pub struct Queue<T> {
    head: AtomicPtr<Node<T>>,
    tail: AtomicPtr<Node<T>>,
}

// SAFETY: the queue owns its values, and hands each to one thread.
unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Default for Queue<T> {
    fn default() -> Queue<T> {
        Queue::new()
    }
}

impl<T> Queue<T> {
    /// An empty queue: one node, the dummy.
    pub fn new() -> Queue<T> {
        let dummy =
            Box::into_raw(Box::new(Node { next: AtomicPtr::new(ptr::null_mut()), value: MaybeUninit::uninit() }));
        Queue { head: AtomicPtr::new(dummy), tail: AtomicPtr::new(dummy) }
    }

    /// Add `value` at the back.
    pub fn enqueue<R: Reclaim>(&self, smr: &mut R, value: T) {
        let node =
            Box::into_raw(Box::new(Node { next: AtomicPtr::new(ptr::null_mut()), value: MaybeUninit::new(value) }));
        smr.enter();
        loop {
            let tail = smr.protect(0, &self.tail);
            // SAFETY: `protect` says the last node may be read.
            let next = unsafe { (*tail).next.load(Ordering::SeqCst) };
            if !next.is_null() {
                // `tail` is a node behind. Help it forward, and look again.
                let _ = self.tail.compare_exchange(tail, next, Ordering::SeqCst, Ordering::SeqCst);
                continue;
            }
            // SAFETY: as above.
            let linked = unsafe { (*tail).next.compare_exchange(next, node, Ordering::SeqCst, Ordering::SeqCst) };
            if linked.is_ok() {
                // If this fails, another thread has already moved `tail` for us.
                let _ = self.tail.compare_exchange(tail, node, Ordering::SeqCst, Ordering::SeqCst);
                smr.exit();
                return;
            }
        }
    }

    /// Take the value at the front, or `None` if the queue is empty.
    pub fn dequeue<R: Reclaim>(&self, smr: &mut R) -> Option<T> {
        smr.enter();
        loop {
            let head = smr.protect(0, &self.head);
            // SAFETY: `protect` says the dummy may be read.
            // `next` may already be retired, if the dummy has been unlinked since we read
            // `head`. Nothing below follows it until the compare-and-swap on `head` has
            // succeeded, which says the dummy was still first, and `next` still linked,
            // at a moment when `next` was already protected.
            let next = smr.protect(1, unsafe { &(*head).next });
            if next.is_null() {
                smr.exit();
                return None;
            }
            let tail = self.tail.load(Ordering::SeqCst);
            if head == tail {
                // `tail` still points at the dummy we are about to retire. Move it first.
                let _ = self.tail.compare_exchange(tail, next, Ordering::SeqCst, Ordering::SeqCst);
                continue;
            }
            if self.head.compare_exchange(head, next, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                // SAFETY: we made `next` the dummy, so its value is ours to move out, and
                // it stays protected until `exit`. The old dummy is ours to retire.
                let value = unsafe { (*next).value.assume_init_read() };
                smr.exit();
                unsafe { smr.retire(head) };
                return Some(value);
            }
        }
    }

    /// How many values are queued. Only for a queue that no other thread is using.
    pub fn len(&mut self) -> usize {
        let mut node = *self.head.get_mut();
        let mut len = 0;
        loop {
            // SAFETY: `&mut self`: no other thread is here to unlink or free it.
            node = unsafe { *(*node).next.get_mut() };
            if node.is_null() {
                return len;
            }
            len += 1;
        }
    }

    pub fn is_empty(&mut self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        // SAFETY: `&mut self`, so the nodes are ours alone. The first is the dummy, whose
        // value is gone or was never there. Every later node still holds its value.
        let mut node = unsafe { Box::from_raw(*self.head.get_mut()) };
        loop {
            let next = *node.next.get_mut();
            if next.is_null() {
                return;
            }
            node = unsafe { Box::from_raw(next) };
            unsafe { node.value.assume_init_drop() };
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::Queue;
    use crate::epoch::Epochs;
    use crate::hazard::Hazards;
    use crate::{AtOnce, Reclaim};
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    use std::thread;
    use std::vec::Vec;

    #[test]
    fn values_come_back_first_in_first_out() {
        let (epochs, hazards) = (Epochs::new(), Hazards::new());
        fn check(smr: &mut impl Reclaim) {
            let mut queue = Queue::new();
            assert_eq!(queue.dequeue(smr), None);
            for value in 0..100 {
                queue.enqueue(smr, value);
            }
            assert_eq!(queue.len(), 100);
            for value in 0..100 {
                assert_eq!(queue.dequeue(smr), Some(value));
            }
            assert!(queue.is_empty());
            assert_eq!(queue.dequeue(smr), None);
            // Each dequeue retires the node that was the dummy before it.
            assert_eq!(smr.counts().retired, 100);
        }
        check(&mut epochs.local(0));
        check(&mut hazards.local(0));
    }

    #[test]
    fn a_queue_that_is_dropped_drops_the_values_left_in_it() {
        let value = std::sync::Arc::new(());
        let mut smr = AtOnce::default();
        let queue = Queue::new();
        for _ in 0..10 {
            queue.enqueue(&mut smr, value.clone());
        }
        assert!(queue.dequeue(&mut smr).is_some());
        assert_eq!(std::sync::Arc::strong_count(&value), 10);
        drop(queue);
        assert_eq!(std::sync::Arc::strong_count(&value), 1);
    }

    /// Two threads enqueue `EACH` numbers of their own, in order, while two others
    /// dequeue. Every number must come out once, and each producer's in the order it put
    /// them in, whoever takes them.
    fn two_in_two_out<R: Reclaim>(local: impl Fn(usize) -> R + Sync) -> crate::Counts {
        const EACH: usize = 20_000;
        let queue = Queue::new();
        let seen: Vec<AtomicU8> = (0..2 * EACH).map(|_| AtomicU8::new(0)).collect();
        let taken = AtomicUsize::new(0);
        let mut counts = crate::Counts::default();
        thread::scope(|s| {
            let threads: Vec<_> = (0..4)
                .map(|thread| {
                    let (queue, seen, taken, local) = (&queue, &seen, &taken, &local);
                    s.spawn(move || {
                        let mut smr = local(thread);
                        if thread < 2 {
                            for i in 0..EACH {
                                queue.enqueue(&mut smr, thread * EACH + i);
                            }
                        } else {
                            let mut last = [None, None];
                            while taken.load(Ordering::Relaxed) < 2 * EACH {
                                let Some(value) = queue.dequeue(&mut smr) else {
                                    thread::yield_now();
                                    continue;
                                };
                                taken.fetch_add(1, Ordering::Relaxed);
                                seen[value].fetch_add(1, Ordering::Relaxed);
                                let last = &mut last[value / EACH];
                                assert!(last.is_none_or(|last| last < value), "a producer's values out of order");
                                *last = Some(value);
                            }
                        }
                        smr.counts()
                    })
                })
                .collect();
            for thread in threads {
                counts += thread.join().unwrap();
            }
        });
        assert!(seen.iter().all(|times| times.load(Ordering::Relaxed) == 1));
        assert_eq!(counts.retired, 2 * EACH);
        counts
    }

    #[test]
    fn two_producers_and_two_consumers_lose_nothing_with_epochs() {
        let mut epochs = Epochs::new();
        let counts = two_in_two_out(|thread| epochs.local(thread));
        assert_eq!(counts.freed + epochs.free_orphans(), counts.retired);
    }

    #[test]
    fn two_producers_and_two_consumers_lose_nothing_with_hazard_pointers() {
        let mut hazards = Hazards::new();
        let counts = two_in_two_out(|thread| hazards.local(thread));
        assert_eq!(counts.freed + hazards.free_orphans(), counts.retired);
    }
}
