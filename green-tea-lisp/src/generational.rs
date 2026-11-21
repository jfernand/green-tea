//! A heap with two generations.
//!
//! The heap's words are one array, as in the other heaps, and it holds two areas. The
//! *old space* starts at word 1 and grows upwards. The *nursery* is at the top of the
//! array. `alloc` takes the next unused words of the nursery and never anything else:
//! every object is born there.
//!
//! When the nursery is full, a *minor collection* copies the objects in it that can be
//! reached to the end of the old space, with Cheney's scan, and the nursery is empty
//! again. It does not trace the old space. It starts from the root stack and from the
//! *remembered set*: the old objects that may hold a reference to a nursery object. An
//! object can come to hold a reference to a younger one in one way only, by a store
//! into it after it was made, and every such store goes through `set_field`. So
//! `set_field` has a *write barrier*: after the store, it puts the object in the
//! remembered set if it must ([`Barrier`]). `alloc` fills in a new object's fields
//! without it, since a new object is in the nursery and is the youngest there is.
//!
//! Every object that survives a minor collection is in the old space afterwards, so no
//! old object refers to the nursery any more, and the remembered set is emptied. This is
//! the arrangement of Andrew Appel's "Simple Generational Garbage Collection and Fast
//! Allocation", down to the size of the nursery, which is half of the words the old
//! space isn't using, or a size the heap was given if that is less. The other half is
//! where the survivors go, so a minor collection can never run out of room.
//!
//! A *major collection* runs after a minor collection that leaves the old space with
//! more than half of the heap's words. The nursery is empty then. It marks what the root
//! stack reaches and slides it down to the start of the heap, as
//! [`crate::mark::MarkHeap`] does when it compacts: the old space needs no second half to
//! copy into. Its mark stack grows as it needs to.

use crate::gc::{Collect, Counts, reference};
use crate::programs::Program;
use crate::{Full, Heap, Kind, Obj, Stats, Value};
use alloc::vec;
use alloc::vec::Vec;

/// Two programs that store into objects that exist, which the five of
/// [`crate::programs::PROGRAMS`] hardly do.
pub const MUTATING: [Program; 2] = [
    Program { name: "counters", source: include_str!("../programs/counters.lisp"), value: "250", cyclic: false },
    Program { name: "slots", source: include_str!("../programs/slots.lisp"), value: "136", cyclic: false },
];

/// What `set_field` does after it has stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Barrier {
    /// Test the store: if the object is in the old space, the value stored is a reference
    /// into the nursery, and the object isn't in the remembered set, put it there. A bit
    /// in the object's header says that it is. This is David Ungar's barrier.
    Filter,
    /// Test nothing: put the object in the remembered set at every store, as often as it
    /// is stored into. The minor collection skips the entries that are nursery objects.
    /// This is Appel's.
    Every,
}

// The header word: the kind, which is never 0 in an object, and three bits.
const KIND_MASK: u64 = 0b11;
/// A major collection has reached this object.
const MARK: u64 = 1 << 2;
/// A minor collection has copied this nursery object: the top half of the header is the
/// index of the copy.
const FORWARDED: u64 = 1 << 3;
/// This old object is in the remembered set ([`Barrier::Filter`]).
const REMEMBERED: u64 = 1 << 4;
/// Also where a major collection writes the index an object will slide to.
const FORWARD_SHIFT: u32 = 32;

/// What a generational heap counts beside [`Stats`] and [`Counts`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Generations {
    pub minor: u64,
    pub major: u64,
    /// Objects that minor collections copied to the old space, and the words in them.
    /// Every other object died in the nursery.
    pub promoted: u64,
    pub promoted_words: u64,
    /// The most objects one minor collection copied.
    pub largest_minor: u64,
    /// Root slots read by minor collections.
    pub minor_roots: u64,
    /// Objects marked by major collections, and the most one of them marked.
    pub marked: u64,
    pub largest_major: u64,
    /// Old objects that major collections found dead.
    pub old_freed: u64,
    /// The most entries the mark stack of a major collection held.
    pub deepest: usize,
    /// Calls of `set_field`.
    pub stores: u64,
    /// Of those, the stores into an object in the old space. Counted by
    /// [`Barrier::Filter`] only: [`Barrier::Every`] tests nothing, and leaves this and
    /// `old_to_young` at 0.
    pub stores_old: u64,
    /// Of those, the stores of a reference to a nursery object.
    pub old_to_young: u64,
    /// Entries added to the remembered set.
    pub recorded: u64,
    /// The most entries the remembered set held when a minor collection started.
    pub remembered_most: usize,
    /// Entries of the remembered set that minor collections read, the old objects among
    /// them, whose fields were then read, and the fields that held a reference into the
    /// nursery.
    pub remembered_read: u64,
    pub remembered_scanned: u64,
    pub remembered_found: u64,
    /// With a clock: the ticks spent in minor collections and in the longest of them,
    /// and the same for major collections.
    pub minor_pause_total: u64,
    pub minor_pause_longest: u64,
    pub major_pause_total: u64,
    pub major_pause_longest: u64,
}

pub struct GenHeap {
    words: Vec<u64>,
    /// The first word after the last old object. The old space is `1..old_top`; word 0
    /// is never used.
    old_top: usize,
    /// Where the nursery starts. It ends where `words` does.
    young: usize,
    /// The first unused word of the nursery.
    free: usize,
    /// The most words the nursery is given.
    nursery: usize,
    barrier: Barrier,
    /// Objects that were stored into, by the index of their header.
    remembered: Vec<u32>,
    roots: Vec<Value>,
    /// Objects in the nursery and in the old space, reachable or not.
    young_objects: u64,
    old_objects: u64,
    /// The mark stack of a major collection.
    stack: Vec<u32>,
    stress: bool,
    clock: Option<fn() -> u64>,
    stats: Stats,
    counts: Counts,
    generations: Generations,
}

impl GenHeap {
    /// A heap of `words` words (8 bytes each) all told, whose nursery is `nursery` words,
    /// or half of the words the old space isn't using when that is less.
    pub fn new(words: usize, nursery: usize, barrier: Barrier) -> GenHeap {
        let mut heap = GenHeap {
            words: vec![0; words + 1],
            old_top: 1,
            young: 0,
            free: 0,
            nursery,
            barrier,
            remembered: Vec::new(),
            roots: Vec::with_capacity(1024),
            young_objects: 0,
            old_objects: 0,
            stack: Vec::new(),
            stress: false,
            clock: None,
            stats: Stats::default(),
            counts: Counts::default(),
            generations: Generations::default(),
        };
        heap.place_nursery();
        heap
    }

    pub fn generations(&self) -> Generations {
        self.generations
    }

    /// Whether `obj` is in the nursery.
    pub fn is_young(&self, obj: Obj) -> bool {
        obj.0 as usize >= self.young
    }

    /// The words in old objects, reachable or not.
    pub fn old_words(&self) -> usize {
        self.old_top - 1
    }

    /// The words the nursery has now, and how many of them are in objects.
    pub fn nursery_words(&self) -> (usize, usize) {
        (self.words.len() - self.young, self.free - self.young)
    }

    /// How many entries the remembered set holds.
    pub fn remembered(&self) -> usize {
        self.remembered.len()
    }

    /// Put the nursery at the top of the heap, empty: half of the words above the old
    /// space, or `nursery` words if that is less. The half below it is where a minor
    /// collection puts what survives.
    fn place_nursery(&mut self) {
        let size = self.nursery.min((self.words.len() - self.old_top) / 2);
        self.young = self.words.len() - size;
        self.free = self.young;
    }

    // ---- headers ----------------------------------------------------------------------

    /// The index of an object's header.
    fn live(&self, obj: Obj) -> usize {
        let at = obj.0 as usize;
        assert!(
            (at >= 1 && at < self.old_top) || (at >= self.young && at < self.free),
            "a stale reference: {at} is in neither generation"
        );
        at
    }

    fn kind_at(&self, at: usize) -> Kind {
        match self.words[at] & KIND_MASK {
            1 => Kind::Pair,
            2 => Kind::Closure,
            3 => Kind::Binding,
            _ => panic!("not an object: {at}"),
        }
    }

    fn header(kind: Kind) -> u64 {
        match kind {
            Kind::Pair => 1,
            Kind::Closure => 2,
            Kind::Binding => 3,
        }
    }

    /// Store into a field as `set_field` does, without the write barrier. A heap must
    /// never do this. It is here to show what the barrier is for.
    pub fn store_without_barrier(&mut self, obj: Obj, i: usize, v: Value) {
        let at = self.live(obj);
        self.words[at + 1 + i] = v.bits();
    }

    // ---- the minor collection ---------------------------------------------------------

    /// The word to store in place of `word`. A reference into the old space stays as it
    /// is. A reference into the nursery becomes a reference to the object's copy in the
    /// old space, which is made now, at `*next`, if there isn't one yet.
    fn promote(&mut self, word: u64, next: &mut usize) -> u64 {
        let Some(at) = reference(word) else { return word };
        if at < self.young {
            return word;
        }
        assert!(at < self.free, "a reference past the last object of the nursery: {at}");
        let header = self.words[at];
        if header & FORWARDED != 0 {
            return Value::obj(Obj((header >> FORWARD_SHIFT) as u32)).bits();
        }
        let size = self.kind_at(at).fields() + 1;
        let copy = *next;
        self.words.copy_within(at..at + size, copy);
        self.words[at] = FORWARDED | (copy as u64) << FORWARD_SHIFT;
        *next += size;
        Value::obj(Obj(copy as u32)).bits()
    }

    /// Copy what can be reached in the nursery to the end of the old space, and empty the
    /// nursery. The old space is not traced: the root stack and the remembered set are
    /// where the collection starts.
    pub fn minor(&mut self) {
        let started = self.clock.map_or(0, |clock| clock());
        let to = self.old_top;
        let mut next = to;
        self.counts.roots += self.roots.len() as u64;
        self.generations.minor_roots += self.roots.len() as u64;
        for slot in 0..self.roots.len() {
            self.roots[slot] = Value::from_bits(self.promote(self.roots[slot].bits(), &mut next));
        }
        // The old objects that were stored into. Whatever they refer to in the nursery
        // is reachable, as far as this collection can tell.
        self.generations.remembered_most = self.generations.remembered_most.max(self.remembered.len());
        self.generations.remembered_read += self.remembered.len() as u64;
        for entry in 0..self.remembered.len() {
            let at = self.remembered[entry] as usize;
            if at >= self.young {
                continue; // a nursery object: the scan gets to it if it can be reached
            }
            self.words[at] &= !REMEMBERED;
            self.generations.remembered_scanned += 1;
            for i in 0..self.kind_at(at).fields() {
                let word = self.words[at + 1 + i];
                if reference(word).is_some_and(|target| target >= self.young) {
                    self.generations.remembered_found += 1;
                    self.words[at + 1 + i] = self.promote(word, &mut next);
                }
            }
        }
        self.remembered.clear();
        // The copies between `scan` and `next` still hold references to the nursery.
        let (mut scan, mut copied) = (to, 0);
        while scan < next {
            let fields = self.kind_at(scan).fields();
            for i in 0..fields {
                self.words[scan + 1 + i] = self.promote(self.words[scan + 1 + i], &mut next);
            }
            scan += fields + 1;
            copied += 1;
        }
        let words = next - to;
        self.stats.freed += self.young_objects - copied;
        self.stats.live_words -= (self.free - self.young) - words;
        self.old_top = next;
        self.old_objects += copied;
        self.young_objects = 0;
        self.place_nursery();

        self.counts.traced += copied;
        self.counts.traced_words += words as u64;
        self.counts.moved_words += words as u64;
        self.generations.minor += 1;
        self.generations.promoted += copied;
        self.generations.promoted_words += words as u64;
        self.generations.largest_minor = self.generations.largest_minor.max(copied);
        if let Some(clock) = self.clock {
            let pause = clock().wrapping_sub(started);
            self.generations.minor_pause_total += pause;
            self.generations.minor_pause_longest = self.generations.minor_pause_longest.max(pause);
        }
        self.counts.finish(copied, 0, self.clock, started);
    }

    // ---- the major collection ---------------------------------------------------------

    fn mark(&mut self, word: u64) -> u64 {
        let Some(at) = reference(word) else { return 0 };
        assert!(at >= 1 && at < self.old_top, "a reference out of the old space: {at}");
        if self.words[at] & MARK != 0 {
            return 0;
        }
        self.words[at] |= MARK;
        self.stack.push(at as u32);
        self.generations.deepest = self.generations.deepest.max(self.stack.len());
        1
    }

    /// Where a marked object is going.
    fn forwarded(&self, word: u64) -> u64 {
        match reference(word) {
            Some(at) => Value::obj(Obj((self.words[at] >> FORWARD_SHIFT) as u32)).bits(),
            None => word,
        }
    }

    /// Mark what the root stack reaches in the old space, and slide it down to the start
    /// of the heap. The nursery must be empty: a major collection follows a minor one.
    pub fn major(&mut self) {
        assert!(
            self.free == self.young && self.remembered.is_empty(),
            "a major collection with objects in the nursery"
        );
        let started = self.clock.map_or(0, |clock| clock());
        self.counts.roots += self.roots.len() as u64;
        let mut marked = 0;
        for slot in 0..self.roots.len() {
            marked += self.mark(self.roots[slot].bits());
        }
        while let Some(at) = self.stack.pop() {
            let at = at as usize;
            for i in 0..self.kind_at(at).fields() {
                marked += self.mark(self.words[at + 1 + i]);
            }
        }
        // First pass: decide where each marked object goes, and write that in its header.
        let (mut at, mut to, mut walked) = (1, 1, 0);
        while at < self.old_top {
            let fields = self.kind_at(at).fields();
            walked += 1;
            if self.words[at] & MARK != 0 {
                self.words[at] |= (to as u64) << FORWARD_SHIFT;
                to += fields + 1;
            } else {
                self.stats.freed += 1;
                self.stats.live_words -= fields + 1;
            }
            at += fields + 1;
        }
        // Second pass: every reference becomes a reference to where its object is going.
        for slot in 0..self.roots.len() {
            self.roots[slot] = Value::from_bits(self.forwarded(self.roots[slot].bits()));
        }
        at = 1;
        while at < self.old_top {
            let fields = self.kind_at(at).fields();
            walked += 1;
            if self.words[at] & MARK != 0 {
                for i in 0..fields {
                    self.words[at + 1 + i] = self.forwarded(self.words[at + 1 + i]);
                }
            }
            at += fields + 1;
        }
        // Third pass: move the objects, each to its own index or a lower one.
        at = 1;
        while at < self.old_top {
            let fields = self.kind_at(at).fields();
            walked += 1;
            if self.words[at] & MARK != 0 {
                let to = (self.words[at] >> FORWARD_SHIFT) as usize;
                let header = self.words[at] & KIND_MASK;
                if to != at {
                    self.words[to] = header;
                    for i in 0..fields {
                        self.words[to + 1 + i] = self.words[at + 1 + i];
                    }
                    self.counts.moved_words += fields as u64 + 1;
                } else {
                    self.words[at] = header;
                }
            }
            at += fields + 1;
        }
        self.old_top = to;
        self.generations.old_freed += self.old_objects - marked;
        self.old_objects = marked;
        self.place_nursery();

        self.counts.traced += marked;
        self.counts.traced_words += to as u64 - 1;
        self.counts.walked += walked;
        self.generations.major += 1;
        self.generations.marked += marked;
        self.generations.largest_major = self.generations.largest_major.max(marked);
        if let Some(clock) = self.clock {
            let pause = clock().wrapping_sub(started);
            self.generations.major_pause_total += pause;
            self.generations.major_pause_longest = self.generations.major_pause_longest.max(pause);
        }
        self.counts.finish(marked, walked, self.clock, started);
    }

    /// Whether the old space has more than half of the heap's words.
    fn old_space_is_large(&self) -> bool {
        self.old_top - 1 > (self.words.len() - 1) / 2
    }

    // ---- a check, for tests -----------------------------------------------------------

    /// Check what the collections rely on: both areas are objects end to end, every
    /// reference in a root slot or a field names an object, and an old object that
    /// refers to the nursery is in the remembered set.
    ///
    /// # Panics
    /// If any of that is not so.
    pub fn check(&self) {
        let mut starts = vec![false; self.words.len()];
        for (from, to) in [(1, self.old_top), (self.young, self.free)] {
            let mut at = from;
            while at < to {
                assert!(self.words[at] & (MARK | FORWARDED) == 0, "a mark or a forwarding pointer was left: {at}");
                starts[at] = true;
                at += self.kind_at(at).fields() + 1;
            }
            assert_eq!(at, to, "an area doesn't end at the end of an object");
        }
        let object = |word: u64| reference(word).inspect(|&at| assert!(starts[at], "a reference to no object: {at}"));
        for slot in &self.roots {
            object(slot.bits());
        }
        for at in (1..self.words.len()).filter(|&at| starts[at]) {
            for i in 0..self.kind_at(at).fields() {
                let Some(target) = object(self.words[at + 1 + i]) else { continue };
                if at < self.young && target >= self.young {
                    assert!(
                        self.remembered.contains(&(at as u32)),
                        "old object {at} refers to nursery object {target} and is not remembered"
                    );
                }
            }
        }
    }
}

impl Collect for GenHeap {
    /// A minor collection and then a major one: afterwards the heap holds what the root
    /// stack reaches, all of it in the old space.
    fn collect(&mut self) {
        self.minor();
        self.major();
    }

    /// `collections` counts minor and major collections alike, `traced` the objects that
    /// the first copied and the second marked, and `walked` the three passes of each
    /// major collection over the old space.
    fn counts(&self) -> Counts {
        self.counts
    }

    fn capacity(&self) -> usize {
        self.words.len() - 1
    }

    fn set_stress(&mut self, stress: bool) {
        self.stress = stress;
    }

    fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = Some(clock);
    }
}

impl Heap for GenHeap {
    fn push(&mut self, v: Value) {
        self.roots.push(v);
    }

    fn pop(&mut self) {
        self.roots.pop().expect("pop from an empty root stack");
    }

    fn roots(&self) -> usize {
        self.roots.len()
    }

    fn root(&self, slot: usize) -> Value {
        self.roots[slot]
    }

    fn set_root(&mut self, slot: usize, v: Value) {
        self.roots[slot] = v;
    }

    fn alloc(&mut self, kind: Kind) -> Result<(), Full> {
        let fields = kind.fields();
        assert!(self.roots.len() >= fields, "alloc without its fields on the root stack");
        if self.stress {
            self.collect();
        } else if self.free + fields + 1 > self.words.len() {
            // The fields of the object to be are on the root stack, so the collection
            // corrects them like any other root.
            self.minor();
            if self.old_space_is_large() {
                self.major();
            }
        }
        if self.free + fields + 1 > self.words.len() {
            return Err(Full);
        }
        let at = self.free;
        self.free += fields + 1;
        let first = self.roots.len() - fields;
        // No barrier: the object is in the nursery, and whatever these fields refer to is
        // at least as old as it is.
        for i in 0..fields {
            self.words[at + 1 + i] = self.roots[first + i].bits();
        }
        self.roots.truncate(first);
        self.words[at] = Self::header(kind);
        self.roots.push(Value::obj(Obj(at as u32)));
        self.young_objects += 1;
        self.stats.allocated += 1;
        self.stats.live_words += fields + 1;
        self.stats.peak_words = self.stats.peak_words.max(self.stats.live_words);
        Ok(())
    }

    fn kind(&self, obj: Obj) -> Kind {
        self.kind_at(self.live(obj))
    }

    fn field(&self, obj: Obj, i: usize) -> Value {
        let at = self.live(obj);
        assert!(i < self.kind_at(at).fields(), "no such field");
        Value::from_bits(self.words[at + 1 + i])
    }

    fn set_field(&mut self, obj: Obj, i: usize, v: Value) {
        let at = self.live(obj);
        assert!(i < self.kind_at(at).fields(), "no such field");
        self.words[at + 1 + i] = v.bits();
        self.generations.stores += 1;
        // The write barrier.
        match self.barrier {
            Barrier::Filter => {
                if at >= self.young {
                    return; // a nursery object: the next minor collection reads all of it
                }
                self.generations.stores_old += 1;
                if !v.as_obj().is_some_and(|target| target.0 as usize >= self.young) {
                    return; // not a reference into the nursery
                }
                self.generations.old_to_young += 1;
                if self.words[at] & REMEMBERED == 0 {
                    self.words[at] |= REMEMBERED;
                    self.remembered.push(at as u32);
                    self.generations.recorded += 1;
                }
            }
            Barrier::Every => {
                self.remembered.push(at as u32);
                self.generations.recorded += 1;
            }
        }
    }

    /// `freed` counts what collections left behind, so `live` and `live_words` include
    /// the objects that have died since the last one.
    fn stats(&self) -> Stats {
        self.stats
    }
}

#[cfg(test)]
#[path = "generational_tests.rs"]
mod tests;
