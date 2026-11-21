#import "../lib.typ": *

= green-tea-smr <crate-smr>

`green-tea-smr` answers one question about lock-free structures: when may a node that was unlinked be freed? A thread can be reading a node at the moment another unlinks it, so the unlinking thread *retires* the node instead, and a `Reclaim` frees it once no thread can still hold its address. The crate has four of them, `AtOnce`, `Leak`, epochs and hazard pointers, and three structures that use them: a Treiber `Stack`, a Michael-Scott `Queue` and `Rcu`.

#spec(
  ("Source", [#repo("green-tea-smr/src/"), 6 files, 974 lines without their tests]),
  ("Depends on", [`alloc`; `no_std`]),
  ("Used by", [`green-tea-coop` (the `smr` kernel)]),
  ("Tests", [23 host tests on real threads, the `smr` kernel]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [crate root], [the `Reclaim` trait, `Counts`, `AtOnce`, `Leak`, `SLOTS` (8)],
  [`epoch`], [`Epochs`, its `Local`],
  [`hazard`], [`Hazards`, its `Local`, `EACH`, `ALL`, `BATCH`],
  [`stack`], [`Stack`, `Node`],
  [`queue`], [`Queue`],
  [`rcu`], [`Rcu`],
)

== The `Reclaim` trait

A structure brackets each operation with `enter` and `exit`, reads every shared pointer it will follow with `protect`, and passes each node it unlinks to `retire`:

#srcfile("green-tea-smr/src/lib.rs", lines: ((89, 89), (91, 91), (96, 96), (99, 99), (106, 106), (108, 109),))[```rust
pub trait Reclaim {
// …
    fn enter(&mut self);
// …
    fn protect<T>(&mut self, index: usize, source: &AtomicPtr<T>) -> *mut T;
// …
    fn exit(&mut self);
// …
    unsafe fn retire<T>(&mut self, node: *mut T);
// …
    fn counts(&self) -> Counts;
}
```]

`protect` takes an `index`, 0 or 1, because a structure may need two nodes at once. `retire` is `unsafe`: the node must have come from `Box::into_raw`, be unreachable to any operation starting now, and be retired once. A retired node is stored as its address and a function that frees it, so one `Reclaim` handles nodes of any type.

`Counts` is what a thread's `Reclaim` has done: `retired`, `freed`, `peak` (the most waiting at once), `operations`, `protected`, `looked` (reads of words belonging to other threads) and `rounds` (epochs advanced, or scans of the hazard pointers). `waiting` is `retired - freed`.

None of this knows what a thread is. A caller gives each thread a number below `SLOTS` and takes a `Local` for it from the shared `Epochs` or `Hazards`. `local` panics if the slot is taken. A `Local` that is dropped leaves its unfreed nodes with the shared object, on a push-only list of *orphans*, and they are freed by `free_orphans`, or when the shared object is dropped. `&mut self` on `free_orphans` is the proof that no `Local` is left.

`AtOnce` frees on the spot, and is wrong: it is there to be seen going wrong. `Leak` never frees. It is safe, and grows with every node retired.

== Memory orderings

Almost every atomic operation in the crate is `SeqCst`, and for most of them that is more than they need. The ones that need it are the two *store, then load* patterns, because neither `Acquire` nor `Release` stops a later load from being performed before an earlier store.

#dtable(columns: (auto, auto, 1fr), ("Operation", "Ordering", "What it protects"),
  [hazard store, then the re-read of `source`], [`SeqCst`], [*Needed.* The scanner's unlink precedes its read of the hazard pointers. In the single total order, either the scanner sees the hazard, or the protecting thread's re-read sees the node gone. With weaker orderings both could miss],
  [`scan`'s hazard loads], [`SeqCst`], [The other half of that argument],
  [announcement store in `enter`, then `protect`'s loads], [`SeqCst`], [*Needed*, the same shape. A thread that reads a pointer must already be visible as active to whoever next checks its slot],
  [`epoch` load and CAS; the announcement load], [`SeqCst`], [The same total order, for the argument in the epoch section],
  [`exit`: the store of quiescent or of null], [`Release`], [The two weaker stores in the crate. Every read of the operation is ordered before the store, so whoever observes the thread as finished also observes that its reads are done],
  [`head`, `tail`, `next` CAS and loads in `Stack`, `Queue`; `current` in `Rcu`], [`SeqCst`], [Publication of a node: its fields are written before the CAS, and a reader that gets the pointer sees them. `AcqRel` and `Acquire` would do for that. `SeqCst` also places the unlink in the order the two arguments above use],
  [`Orphans` push], [`SeqCst`], [Ownership of the list node. `AcqRel` would do],
  [`taken` swap and store], [`SeqCst`], [One `Local` for a slot at a time],
)

== Epochs

`epoch` is Trevor Brown's DEBRA. There is one shared count, the *epoch*, and one shared word for each thread, its *announcement*: the epoch it saw when its present operation began, or a low bit that says it is between operations (*quiescent*). The epoch counts in twos to leave that bit free. A thread keeps its retired nodes in three *limbo bags*, one for each of its last three announcements.

#srcfile("green-tea-smr/src/epoch.rs", lines: ((134, 158),), notes: ((138, [The thread sees a new epoch: its oldest bag can be freed.]), (146, [One other thread's announcement per operation, so a thread pays a constant amount, and the cost of a full round is spread over `SLOTS` operations.]), (149, [All `SLOTS` agreed with this epoch, so try to advance it. Nobody is in charge: whoever gets there first.]), (157, [The store that makes this thread active. Every pointer the operation reads, it reads after this.]),), size: 6.3pt)[```rust
fn enter(&mut self) {
    self.counts.operations += 1;
    let shared = self.shared;
    let epoch = shared.epoch.0.load(Ordering::SeqCst);
    if epoch != self.announced {
        self.announced = epoch;
        self.checking = 0;
        self.rotate();
    }
    // Look at one thread's announcement. It agrees if the thread is between
    // operations, or began its operation in this epoch.
    self.counts.looked += 1;
    let other = shared.announcements[self.checking % SLOTS].0.load(Ordering::SeqCst);
    if other & QUIESCENT != 0 || other == epoch {
        self.checking += 1;
        if self.checking >= SLOTS
            && shared.epoch.0.compare_exchange(epoch, epoch + EPOCH, Ordering::SeqCst, Ordering::SeqCst).is_ok()
        {
            self.counts.rounds += 1;
        }
    }
    // Announce the epoch, and that an operation is under way. Every pointer this
    // operation reads, it reads after this store.
    shared.announcements[self.slot].0.store(epoch, Ordering::SeqCst);
}
```]

`rotate` moves to the oldest bag and frees it:

#srcfile("green-tea-smr/src/epoch.rs", lines: ((112, 125),))[```rust
fn rotate(&mut self) {
    self.current = (self.current + 1) % 3;
    self.counts.freed += self.bags[self.current].len();
    for node in self.bags[self.current].drain(..) {
        // SAFETY: this bag was the current one three announcements ago, so this
        // thread has seen the epoch change three times since the last of these nodes
        // was retired, and the epoch has changed twice since then. The second change
        // needed every thread to be quiescent, or to have announced the epoch the
        // first change made, which a thread does only on its way out of quiescence.
        // So every thread has been quiescent since the node was unlinked, and none
        // can have found it afterwards.
        unsafe { node.free() };
    }
}
```]

The safety argument, which the comment in `rotate` gives, goes like this. A thread T that stores its announcement at time $s$ may have read the epoch long before, so its announcement can be stale. A checker that passed T's slot before $s$ can still complete *one* more advance. But the checker of the next epoch read that epoch after $s$, so it reads T's slot after $s$, finds T active with an older epoch, and stops. So while T is active the epoch advances at most once. A node that T can reach was unlinked after $s$. The retiring thread frees it after its third rotation, by which time the epoch has advanced at least twice since the node was retired, and that can't have happened while T was still active.

`is_active` says whether this thread is in an operation, and `Epochs::epoch` how many times the epoch has moved on.

The cost: two loads and two stores for a whole operation, whatever it reads. And a thread that stops in the middle of an operation stops the epoch, so from then on no thread frees anything.

== Hazard pointers

`hazard` follows Maged Michael. Each thread has `EACH` = 2 shared words that only it writes. Before a thread follows a pointer read from a shared place, it writes the pointer to one of them, and reads the place again:

#srcfile("green-tea-smr/src/hazard.rs", lines: ((89, 104),), notes: ((94, [A test hook, for a node freed between the read and the store.]), (95, [The hazard is announced.]), (98, [If the place still holds the pointer, the node was linked at a moment when the hazard already named it, so from then on nobody will free it.]),), size: 6.3pt)[```rust
pub fn protect_paused<T>(&mut self, index: usize, source: &AtomicPtr<T>, mut pause: impl FnMut()) -> *mut T {
    self.counts.protected += 1;
    let hazard = &self.shared.slots[self.slot].0[index];
    let mut node = source.load(Ordering::SeqCst);
    loop {
        pause();
        hazard.store(node as *mut u8, Ordering::SeqCst);
        // The node may have been unlinked, retired and freed between the load and
        // the store. If `source` still holds it now, it hadn't been.
        let again = source.load(Ordering::SeqCst);
        if again == node {
            return node;
        }
        node = again;
    }
}
```]

A thread keeps its retired nodes in a list. When the list reaches `BATCH` nodes, twice `ALL` (16), so 32, `scan` reads every hazard pointer once and frees every node none of them names:

#srcfile("green-tea-smr/src/hazard.rs", lines: ((107, 130),), notes: ((116, [Sorted, so each retired node costs a binary search.]), (119, [At most 16 can be named, so a scan of 32 frees at least half.]),))[```rust
pub fn scan(&mut self) {
    self.counts.rounds += 1;
    let mut named = [ptr::null_mut(); ALL];
    for (slot, hazards) in self.shared.slots.iter().enumerate() {
        for (index, hazard) in hazards.0.iter().enumerate() {
            named[slot * EACH + index] = hazard.load(Ordering::SeqCst);
        }
    }
    self.counts.looked += ALL;
    named.sort_unstable();
    let mut next = 0;
    while next < self.retired.len() {
        if named.binary_search(&self.retired[next].node).is_ok() {
            next += 1;
            continue;
        }
        self.counts.freed += 1;
        // SAFETY: the node was unlinked before it was retired, and no hazard pointer
        // named it when we looked, after that. A thread that names it later read its
        // address from somewhere it no longer is, and will find that out when it
        // reads the place again.
        unsafe { self.retired.swap_remove(next).free() };
    }
}
```]

`protect_paused` and `scan` are public for the tests. The cost is a store and a second load for *every pointer followed*. What it buys is a bound: a thread that stops can keep `EACH` nodes from being freed, and no more.

== The structures

=== `Stack`

A Treiber stack: a linked list and a pointer to its first node, changed only by compare-and-swap. A node holds `next` and a `ManuallyDrop` value, moved out by the pop that unlinks it, so freeing the node doesn't drop the value. `push` needs no protection, because it only stores `head`:

#srcfile("green-tea-smr/src/stack.rs", lines: ((69, 89),), notes: ((72, [The node may be read until `exit`. Under `AtOnce` it may not.]), (78, [The window a test holds open with `pause`.]), (80, [The compare-and-swap compares addresses only. A node freed and reallocated at the same address passes it: the ABA problem, which the `Reclaim` is there to prevent.]), (85, [`exit` first: the node goes to `retire` with this thread's own protection dropped.]),))[```rust
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
```]

`pop` is `pop_paused` with an empty pause. The rest is for tests and kernels: `head` exposes the pointer for manual protection, `look` copies the nodes out, `len` counts them up to a limit because an ABA can tie the list into a loop, `is_empty`, and `discard` and `abandon`, which dispose of nodes that a deliberately corrupted stack can no longer free once each. `Stack::new` is `const`.

=== `Queue`

A Michael-Scott queue: a linked list with a pointer to each end, and a *dummy* node at the front that holds no value, so `enqueue` and `dequeue` never work on the same pointer. `enqueue` links a node after the last one and then swings `tail`, in two compare-and-swaps. A thread that finds `tail` one node behind moves it forward first, so nobody waits for a thread that stopped between the two:

#srcfile("green-tea-smr/src/queue.rs", lines: ((51, 73),), size: 6.3pt)[```rust
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
```]

`dequeue` swings `head` to the second node, takes its value, and retires the old dummy. It follows two pointers and so uses both protection slots:

#srcfile("green-tea-smr/src/queue.rs", lines: ((76, 105),), notes: ((85, [`next` may already be retired, if the dummy was unlinked since `head` was read. Nothing follows it until the CAS on `head` has succeeded, which proves the dummy was still first, and `next` still linked, while `next` was protected.]), (91, [`tail` must not be left pointing at the dummy that is about to be retired.]), (99, [The second node is the new dummy. Its value is moved out, and only the winner of the CAS reads it.]),))[```rust
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
```]

`Queue::new` allocates the first dummy. `len` and `is_empty` take `&mut self`, and are only for a queue no other thread is using.

=== `Rcu`

Read, copy, update: a value many threads read, and that is never changed in place. A reader follows one pointer. A writer makes a new value from the old one and swings the pointer:

#srcfile("green-tea-smr/src/rcu.rs", lines: ((34, 60),), notes: ((45, [`f` is `Fn`, because a lost race calls it again on the value the winner left.]), (51, [A reader sees the old value whole or the new value whole.]), (54, [The old value waits for the `Reclaim`.]),))[```rust
pub fn read<R: Reclaim, V>(&self, smr: &mut R, f: impl FnOnce(&T) -> V) -> V {
    smr.enter();
    let current = smr.protect(0, &self.current);
    // SAFETY: `protect` says the value may be read until `exit`.
    let result = f(unsafe { &*current });
    smr.exit();
    result
}

/// Replace the value with what `f` makes of it. If another writer gets in first, `f`
/// is called again on the value that writer left.
pub fn update<R: Reclaim>(&self, smr: &mut R, f: impl Fn(&T) -> T) {
    smr.enter();
    loop {
        let old = smr.protect(0, &self.current);
        // SAFETY: as for `read`.
        let new = Box::into_raw(Box::new(f(unsafe { &*old })));
        if self.current.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            smr.exit();
            // SAFETY: the compare-and-swap unlinked it, and it came from a `Box`.
            unsafe { smr.retire(old) };
            return;
        }
        // SAFETY: nobody else has seen the copy we made.
        drop(unsafe { Box::from_raw(new) });
    }
}
```]

`Rcu::new` boxes the first value. This is the shape of the Linux kernel's RCU, with `enter` and `exit` where the kernel has `rcu_read_lock` and `rcu_read_unlock`, and `retire` where it has `call_rcu`. The kernel's readers pay less: there, a reader announces nothing.

== How it is exercised

The 23 tests include:

- the interrupted pop by hand, under epochs and under hazard pointers;
- `four_threads_pop_every_value_once_*`, for both schemes;
- `two_producers_and_two_consumers_lose_nothing_*`, for both schemes;
- `a_thread_that_stops_in_an_operation_stops_every_free`.

#output(title: "cargo test -p green-tea-smr")[
```
test result: ok. 23 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `smr` kernel starts with the ABA problem, step by step, on real addresses (shown as offsets):

#page(flipped: true)[#output(title: "cargo xtask kernel --kernel smr --target riscv64", size: 6.6pt)[
```
a stack holds 3, 2 and 1. a pop reads the first node's address and the second's, and before its
compare-and-swap another thread pops twice, pushes 9 on another stack, and pushes 4 on this one.
  nodes freed at once:
    the stack is +64 (3) -> +32 (2) -> +0 (1); the pop has read head = +64 and next = +32
    the other thread pops 3 and 2, and both nodes are freed; its 9 goes in a node at +32, and its 4 in one at +64
    the compare-and-swap of head from +64 to +32 succeeds; the pop returns 4
    the stack is +32 (9), which is the other stack's node; 1 is on no stack
  nodes freed by epochs:
    the stack is +64 (3) -> +32 (2) -> +0 (1); the pop has read head = +64 and next = +32
    the other thread pops 3 and 2, and both nodes wait in a limbo bag; its 9 goes in a node at +176, and its 4 in one at +208
    the compare-and-swap of head from +64 to +32 fails, and the pop starts again; the pop returns 4
    the stack is +0 (1)
  nodes freed by hazard pointers:
    the stack is +64 (3) -> +32 (2) -> +0 (1); the pop has read head = +64 and next = +32
    the other thread pops 3 and 2; a hazard pointer names the first, and the second is freed; its 9 goes in a node at +32, and its 4 in one at +176
    the compare-and-swap of head from +64 to +32 fails, and the pop starts again; the pop returns 4
    the stack is +0 (1)
```
]]

Freed at once, the allocator reuses both addresses, the CAS succeeds on a stack that has changed, and the stack ends up holding another stack's node. Under both real schemes the first node's address can't be reused while the pop holds it, so the CAS fails as it should.

The second experiment is what each scheme does when a thread stops:

#output(title: "the same run", size: 6.6pt)[
```
CPU 3 stops in the middle of a pop, and the other three then make 2000 pops and pushes each. when they are done:
  freed by epochs          6000 nodes were retired, 0 freed, and 6000 waited: 192000 bytes of nodes
  this run: the heap had 291008 bytes more in use, with the lists of them
  freed by hazard pointers 6000 nodes were retired, and at most 96 waited: 3072 bytes of nodes
  this run: 5952 were freed, and 48 waited at the end
  this run: the heap had 3120 bytes more in use, with the lists of them
```
]

One stopped thread and epochs free nothing. Hazard pointers bound the damage. The kernel also runs both schemes on the stack and queue, the RCU pair, and a work queue of jobs:

#output(title: "the same run", size: 6.8pt)[
```
each CPU pushes 5000 values of its own on one stack, and pops one after each push:
  freed by epochs          20000 values came back, each once; 20000 nodes were retired, and 20000 freed
  freed by hazard pointers 20000 values came back, each once; 20000 nodes were retired, and 20000 freed
CPU 0 replaces a pair of numbers 5000 times, by copying it, while the other three read it:
  every read saw one whole pair, none older than the read before; 5000 old pairs were retired, and 5000 freed
  this run: 157133 reads; 54 ms
a queue holds one job, to count the primes below 32768. each CPU takes a job, and splits it in two
if it covers more than 256 numbers:
  255 jobs ran: 127 were split, and 128 counted 3512 primes; 255 nodes were retired, and 255 freed
  this run: the CPUs ran 56, 58, 86 and 55 jobs; 63 ms, and one CPU counting alone took 249 ms
```
]

== Limits

- At most `SLOTS`, 8, threads can share an `Epochs` or a `Hazards`.
- Epochs stop freeing while any thread is stopped inside an operation, without bound on the memory held.
- Orphans are freed only when the owner of the `Epochs` or `Hazards` has exclusive access, so a program whose threads come and go accumulates them.
- A `read` or `update` whose closure panics never calls `exit`. Under epochs the thread stays announced until its `Local` is dropped, and until then the epoch can't advance.
- *Defect.* `Counts`' `AddAssign` adds the `peak` fields (`lib.rs:78`). A sum of per-thread peaks is an upper bound on the peak of the sum, and is reported in a field documented as "the most nodes that were retired and not yet freed, at any one time".
- *Defect.* `Rcu::update` retires a `T` with no `T: Send` bound (`rcu.rs:45`). A retired value left as an orphan is dropped on whichever thread frees the orphans, so an `Rcu<Rc<_>>`, which compiles and runs, can have its `Rc` dropped on a different thread from its other clones. `Stack` and `Queue` don't have the problem, because their nodes never drop a value.

Four answers to when, and what each costs. @crate-lisp is a program whose heap answers it by tracing.
