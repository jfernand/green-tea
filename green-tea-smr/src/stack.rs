//! A lock-free stack, after R. Kent Treiber: a linked list and a pointer to its first
//! node, changed only by compare-and-swap.
//!
//! `push` makes a node that points at the present first node, and swings the head to it
//! if the head hasn't changed. `pop` reads the first node's `next`, and swings the head
//! to that if the head hasn't changed. Neither waits for another thread: a
//! compare-and-swap that fails means another thread's succeeded, and the loop goes round.
//!
//! `pop` is where reclamation comes in. Between its read of the head and its
//! compare-and-swap it reads a field of a node that another thread may pop, and free, at
//! any moment. And the compare-and-swap only compares addresses: if the node was freed
//! and its address handed out again for a new node, it succeeds on a stack that has
//! changed (the ABA problem). Both are the [`Reclaim`]'s to prevent.

use crate::Reclaim;
use alloc::boxed::Box;
use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

pub struct Node<T> {
    next: *mut Node<T>,
    /// Moved out by the `pop` that unlinks the node, and so not dropped with it.
    value: ManuallyDrop<T>,
}

pub struct Stack<T> {
    head: AtomicPtr<Node<T>>,
}

// SAFETY: the stack owns its values, and hands each to one thread.
unsafe impl<T: Send> Send for Stack<T> {}
unsafe impl<T: Send> Sync for Stack<T> {}

impl<T> Default for Stack<T> {
    fn default() -> Stack<T> {
        Stack::new()
    }
}

impl<T> Stack<T> {
    pub const fn new() -> Stack<T> {
        Stack { head: AtomicPtr::new(ptr::null_mut()) }
    }

    /// Put `value` on top. Returns the address of the node that holds it, for a test to
    /// print: it may be freed at any time.
    pub fn push(&self, value: T) -> *const Node<T> {
        let node = Box::into_raw(Box::new(Node { next: ptr::null_mut(), value: ManuallyDrop::new(value) }));
        let mut head = self.head.load(Ordering::SeqCst);
        loop {
            // SAFETY: the node is ours until the compare-and-swap below succeeds. `head`
            // is only stored, never followed, so `push` protects nothing.
            unsafe { (*node).next = head };
            match self.head.compare_exchange(head, node, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return node,
                Err(seen) => head = seen,
            }
        }
    }

    /// Take the value on top, or `None` if the stack is empty.
    pub fn pop<R: Reclaim>(&self, smr: &mut R) -> Option<T> {
        self.pop_paused(smr, || ())
    }

    /// `pop`, calling `pause` between the read of the first node's `next` and the
    /// compare-and-swap that depends on it. A test does there what another thread could.
    pub fn pop_paused<R: Reclaim>(&self, smr: &mut R, mut pause: impl FnMut()) -> Option<T> {
        smr.enter();
        loop {
            let head = smr.protect(0, &self.head);
            if head.is_null() {
                smr.exit();
                return None;
            }
            // SAFETY: `protect` says the node may be read. Under `AtOnce` it may not.
            let next = unsafe { (*head).next };
            pause();
            if self.head.compare_exchange(head, next, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                // SAFETY: the compare-and-swap unlinked the node, so its value is ours to
                // move out, and the node is ours to retire.
                let value = unsafe { ptr::read(&(*head).value) };
                smr.exit();
                unsafe { smr.retire(head) };
                return Some(ManuallyDrop::into_inner(value));
            }
        }
    }

    /// The pointer to the first node, for a caller that wants to `protect` it by hand.
    pub fn head(&self) -> &AtomicPtr<Node<T>> {
        &self.head
    }

    /// Write the address and the value of each node, from the top, into `nodes`, and
    /// return how many there were, counting at most `nodes.len()`. For a test to print,
    /// and only for a stack that no other thread is using.
    pub fn look(&mut self, nodes: &mut [(usize, T)]) -> usize
    where
        T: Copy,
    {
        let mut node = *self.head.get_mut();
        let mut len = 0;
        while !node.is_null() && len < nodes.len() {
            // SAFETY: `&mut self`: no other thread is here to unlink or free it.
            nodes[len] = (node as usize, unsafe { *(*node).value });
            node = unsafe { (*node).next };
            len += 1;
        }
        len
    }

    /// How many nodes are linked, counting at most `limit`. A stack that an ABA has tied
    /// into a loop has no end to find.
    pub fn len(&mut self, limit: usize) -> usize {
        let mut node = *self.head.get_mut();
        let mut len = 0;
        while !node.is_null() && len < limit {
            // SAFETY: as for `look`.
            node = unsafe { (*node).next };
            len += 1;
        }
        len
    }

    pub fn is_empty(&mut self) -> bool {
        self.head.get_mut().is_null()
    }

    /// Free a node that is linked nowhere and that nobody else will free: one that an ABA
    /// cut out of its stack.
    ///
    /// # Safety
    /// `node` came from `push`, on a stack of this type, and all of the above is true.
    pub unsafe fn discard(node: *const Node<T>) {
        let mut owned = unsafe { Box::from_raw(node as *mut Node<T>) };
        unsafe { ManuallyDrop::drop(&mut owned.value) };
    }

    /// Forget every node without freeing any. For a stack that a test has corrupted on
    /// purpose, whose nodes can no longer be freed once each.
    pub fn abandon(&mut self) {
        *self.head.get_mut() = ptr::null_mut();
    }
}

impl<T> Drop for Stack<T> {
    fn drop(&mut self) {
        let mut node = *self.head.get_mut();
        while !node.is_null() {
            // SAFETY: `&mut self`, so the nodes are ours alone, each linked once.
            let mut owned = unsafe { Box::from_raw(node) };
            unsafe { ManuallyDrop::drop(&mut owned.value) };
            node = owned.next;
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::Stack;
    use crate::epoch::Epochs;
    use crate::hazard::Hazards;
    use crate::{AtOnce, Reclaim};
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::thread;
    use std::vec::Vec;

    #[test]
    fn values_come_back_last_in_first_out() {
        let (epochs, hazards) = (Epochs::new(), Hazards::new());
        fn check(smr: &mut impl Reclaim) {
            let mut stack = Stack::new();
            assert_eq!(stack.pop(smr), None);
            for value in 0..100 {
                stack.push(value);
            }
            assert_eq!(stack.len(1000), 100);
            for value in (0..100).rev() {
                assert_eq!(stack.pop(smr), Some(value));
            }
            assert!(stack.is_empty());
            assert_eq!(smr.counts().retired, 100);
        }
        check(&mut epochs.local(0));
        check(&mut hazards.local(0));
    }

    #[test]
    fn a_stack_that_is_dropped_drops_the_values_left_on_it() {
        let value = std::sync::Arc::new(());
        let stack = Stack::new();
        for _ in 0..10 {
            stack.push(value.clone());
        }
        assert!(stack.pop(&mut AtOnce::default()).is_some());
        assert_eq!(std::sync::Arc::strong_count(&value), 10);
        drop(stack);
        assert_eq!(std::sync::Arc::strong_count(&value), 1);
    }

    /// What another thread does between a `pop`'s read of `next` and its
    /// compare-and-swap, in the worst case: pops that node and the one under it, and
    /// pushes a new one. The `pop` must then take the new node, and leave the third.
    fn interrupted_pop<R: Reclaim>(mine: &mut R, theirs: &mut R, between: impl Fn(&mut R)) {
        let mut stack = Stack::new();
        for value in [1, 2, 3] {
            stack.push(value);
        }
        let mut done = false;
        let popped = stack.pop_paused(mine, || {
            if !core::mem::replace(&mut done, true) {
                assert_eq!(stack.pop(theirs), Some(3));
                assert_eq!(stack.pop(theirs), Some(2));
                between(theirs);
                stack.push(4);
            }
        });
        assert_eq!(popped, Some(4));
        assert_eq!(stack.len(10), 1);
        assert_eq!(stack.pop(mine), Some(1));
        assert_eq!(stack.pop(mine), None);
    }

    #[test]
    fn a_pop_interrupted_by_two_pops_and_a_push_takes_the_new_node_with_epochs() {
        let epochs = Epochs::new();
        let (mut mine, mut theirs) = (epochs.local(0), epochs.local(1));
        interrupted_pop(&mut mine, &mut theirs, |_| ());
        // Nothing the other thread retired while ours was in its operation is freed.
        assert_eq!((theirs.counts().retired, theirs.counts().freed), (2, 0));
    }

    #[test]
    fn a_pop_interrupted_by_two_pops_and_a_push_takes_the_new_node_with_hazard_pointers() {
        let hazards = Hazards::new();
        let (mut mine, mut theirs) = (hazards.local(0), hazards.local(1));
        interrupted_pop(&mut mine, &mut theirs, |theirs| theirs.scan());
        // The scan freed the second node. The first was named by our hazard pointer.
        assert_eq!((theirs.counts().retired, theirs.counts().freed), (2, 1));
    }

    /// Four threads each push a number of their own and pop one, `EACH` times. Every
    /// number pushed must be popped once, by someone.
    fn four_threads<R: Reclaim>(local: impl Fn(usize) -> R + Sync) -> crate::Counts {
        const EACH: usize = 20_000;
        let stack = Stack::new();
        let popped: Vec<AtomicU8> = (0..4 * EACH).map(|_| AtomicU8::new(0)).collect();
        let mut counts = crate::Counts::default();
        thread::scope(|s| {
            let threads: Vec<_> = (0..4)
                .map(|thread| {
                    let (stack, popped, local) = (&stack, &popped, &local);
                    s.spawn(move || {
                        let mut smr = local(thread);
                        for i in 0..EACH {
                            stack.push(thread * EACH + i);
                            let value = stack.pop(&mut smr).expect("a thread pops after it has pushed");
                            popped[value].fetch_add(1, Ordering::Relaxed);
                        }
                        smr.counts()
                    })
                })
                .collect();
            for thread in threads {
                counts += thread.join().unwrap();
            }
        });
        assert!(popped.iter().all(|times| times.load(Ordering::Relaxed) == 1));
        assert_eq!(counts.retired, 4 * EACH);
        counts
    }

    #[test]
    fn four_threads_pop_every_value_once_with_epochs() {
        let mut epochs = Epochs::new();
        let counts = four_threads(|thread| epochs.local(thread));
        assert_eq!(counts.freed + epochs.free_orphans(), counts.retired);
    }

    #[test]
    fn four_threads_pop_every_value_once_with_hazard_pointers() {
        let mut hazards = Hazards::new();
        let counts = four_threads(|thread| hazards.local(thread));
        assert_eq!(counts.freed + hazards.free_orphans(), counts.retired);
        assert!(counts.peak <= 4 * crate::hazard::BATCH);
    }
}
