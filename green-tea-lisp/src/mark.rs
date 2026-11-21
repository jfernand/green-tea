//! A heap that marks, and then sweeps or compacts.
//!
//! The heap is an array of words. An object is a header word followed by its fields, and
//! a reference to it is the index of its header, as in [`crate::rc::RcHeap`]. The header
//! holds the object's kind and one *mark bit*, and no count: `push`, `pop`, `set_root` and
//! `set_field` store a word and do nothing else.
//!
//! A collection has two phases. The *mark* phase sets the mark bit of every object that
//! can be reached from the root stack ([`MarkHeap::mark_roots`], [`MarkHeap::mark_some`]).
//! Objects that are marked and whose fields haven't been read yet wait on the *mark
//! stack*, which has a fixed size: if an object doesn't fit, it is marked all the same,
//! and the heap is searched afterwards for marked objects that refer to unmarked ones.
//!
//! The second phase is a choice ([`Reclaim`]). A *sweep* walks the heap from one end to
//! the other, puts every unmarked object on the free list for its size, and clears the
//! marks; `alloc` takes from those lists, as `RcHeap`'s does. A *compaction* slides the
//! marked objects down to the start of the heap, in order, and corrects every reference to
//! them; `alloc` then takes the next unused words, and there are no free lists at all.
//!
//! A collection runs when `alloc` finds no room, and at no other time unless
//! [`Collect::set_stress`] or [`Collect::collect`] asks.

use crate::gc::{Collect, Counts, reference};
use crate::{Full, Heap, Kind, Obj, Stats, Value};
use alloc::vec;
use alloc::vec::Vec;

/// What a collection does with the objects it didn't mark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reclaim {
    /// Put them on free lists, where they are. Nothing moves.
    Sweep,
    /// Slide the marked objects down over them. References change.
    Compact,
}

// The header word. The kind is never cleared, so the heap can be walked object by object
// whether or not they are allocated.
const KIND_MASK: u64 = 0b11;
/// Reached by the collection that is running.
const MARK: u64 = 1 << 2;
/// On a free list.
const FREE: u64 = 1 << 3;
/// During a compaction, the top half of a marked object's header is where it will go.
const FORWARD_SHIFT: u32 = 32;

pub struct MarkHeap {
    words: Vec<u64>,
    /// The first word never yet used, or, after a compaction, the first word after the
    /// last object. Word 0 is never used, so that 0 can end a list.
    next: usize,
    /// [`Reclaim::Sweep`]: the free lists, one for objects of two fields and one for
    /// three, linked through the first field, lowest address first.
    free: [usize; 2],
    roots: Vec<Value>,
    reclaim: Reclaim,
    /// Objects that are marked and whose fields haven't been read: the gray ones.
    stack: Vec<u32>,
    stack_limit: usize,
    /// An object was marked while the stack was full.
    overflowed: bool,
    stress: bool,
    clock: Option<fn() -> u64>,
    stats: Stats,
    counts: Counts,
    /// What the collection that is running has traced and walked.
    traced: u64,
    walked: u64,
}

impl MarkHeap {
    /// A heap of `words` words (8 bytes each), all of them unused, with a mark stack of
    /// `mark_stack` entries (4 bytes each) beside it.
    pub fn new(words: usize, reclaim: Reclaim, mark_stack: usize) -> MarkHeap {
        MarkHeap {
            words: vec![0; words + 1],
            next: 1,
            free: [0; 2],
            roots: Vec::with_capacity(1024),
            reclaim,
            stack: Vec::with_capacity(mark_stack),
            stack_limit: mark_stack,
            overflowed: false,
            stress: false,
            clock: None,
            stats: Stats::default(),
            counts: Counts::default(),
            traced: 0,
            walked: 0,
        }
    }

    /// How many words have ever been used or, after a compaction, are in use.
    pub fn touched(&self) -> usize {
        self.next - 1
    }

    /// Whether `obj` is marked. Meaningful between `mark_roots` and the end of a collection.
    pub fn is_marked(&self, obj: Obj) -> bool {
        self.words[obj.0 as usize] & MARK != 0
    }

    // ---- headers ----------------------------------------------------------------------

    /// The index of an allocated object's header.
    fn live(&self, obj: Obj) -> usize {
        let at = obj.0 as usize;
        assert!(at >= 1 && at < self.next, "not an object: {at}");
        assert!(self.words[at] & FREE == 0, "a freed object was used: {at}");
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

    // ---- the mark phase ---------------------------------------------------------------

    /// If `word` is a reference to an object that isn't marked, mark the object, and put
    /// it on the mark stack if there is room.
    fn mark(&mut self, word: u64) {
        let Some(at) = reference(word) else { return };
        assert!(at >= 1 && at < self.next && self.words[at] & FREE == 0, "a reference to a freed object: {at}");
        if self.words[at] & MARK != 0 {
            return;
        }
        self.words[at] |= MARK;
        self.traced += 1;
        self.counts.traced_words += self.kind_at(at).fields() as u64 + 1;
        if self.stack.len() < self.stack_limit {
            self.stack.push(at as u32);
            self.counts.deepest = self.counts.deepest.max(self.stack.len());
        } else {
            self.overflowed = true;
            self.counts.overflows += 1;
        }
    }

    /// Start a collection: mark what the root stack refers to.
    pub fn mark_roots(&mut self) {
        self.traced = 0;
        self.walked = 0;
        self.counts.roots += self.roots.len() as u64;
        for slot in 0..self.roots.len() {
            self.mark(self.roots[slot].bits());
        }
    }

    /// Take up to `budget` objects off the mark stack and mark what their fields refer
    /// to. Returns whether there is more to do: `false` means that every object that can
    /// be reached from the root stack is marked.
    pub fn mark_some(&mut self, budget: usize) -> bool {
        let mut done = 0;
        loop {
            while let Some(at) = self.stack.pop() {
                let at = at as usize;
                for i in 0..self.kind_at(at).fields() {
                    self.mark(self.words[at + 1 + i]);
                }
                done += 1;
                if done == budget {
                    return true;
                }
            }
            if !self.overflowed {
                return false;
            }
            // Some object was marked and couldn't be remembered. Its fields are still to
            // be read, and all that says so is that they refer to unmarked objects.
            self.overflowed = false;
            self.counts.rescans += 1;
            let mut at = 1;
            while at < self.next {
                let fields = self.kind_at(at).fields();
                self.walked += 1;
                if self.words[at] & MARK != 0 {
                    for i in 0..fields {
                        self.mark(self.words[at + 1 + i]);
                    }
                }
                at += fields + 1;
            }
        }
    }

    // ---- the sweep --------------------------------------------------------------------

    /// Free every object that isn't marked, clear the marks, and build the free lists again.
    fn sweep(&mut self) {
        self.free = [0; 2];
        let mut last = [0; 2];
        let mut at = 1;
        while at < self.next {
            let fields = self.kind_at(at).fields();
            self.walked += 1;
            if self.words[at] & MARK != 0 {
                self.words[at] &= !MARK;
            } else {
                if self.words[at] & FREE == 0 {
                    self.stats.freed += 1;
                    self.stats.live_words -= fields + 1;
                }
                self.words[at] = self.words[at] & KIND_MASK | FREE;
                self.words[at + 1] = 0;
                let list = fields - 2;
                if last[list] == 0 {
                    self.free[list] = at;
                } else {
                    self.words[last[list] + 1] = at as u64;
                }
                last[list] = at;
            }
            at += fields + 1;
        }
    }

    // ---- the compaction ---------------------------------------------------------------

    /// Where a marked object is going.
    fn forwarded(&self, word: u64) -> u64 {
        match reference(word) {
            Some(at) => Value::obj(Obj((self.words[at] >> FORWARD_SHIFT) as u32)).bits(),
            None => word,
        }
    }

    /// Slide the marked objects down to the start of the heap, keeping their order.
    fn compact(&mut self) {
        // First pass: decide where each marked object goes, and write that in its header.
        let (mut at, mut to) = (1, 1);
        while at < self.next {
            let fields = self.kind_at(at).fields();
            self.walked += 1;
            if self.words[at] & MARK != 0 {
                self.words[at] |= (to as u64) << FORWARD_SHIFT;
                to += fields + 1;
            } else {
                self.stats.freed += 1;
                self.stats.live_words -= fields + 1;
            }
            at += fields + 1;
        }
        // Second pass: every reference, in a root slot or in a field of a marked object,
        // becomes a reference to where its object is going.
        for slot in 0..self.roots.len() {
            self.roots[slot] = Value::from_bits(self.forwarded(self.roots[slot].bits()));
        }
        at = 1;
        while at < self.next {
            let fields = self.kind_at(at).fields();
            self.walked += 1;
            if self.words[at] & MARK != 0 {
                for i in 0..fields {
                    self.words[at + 1 + i] = self.forwarded(self.words[at + 1 + i]);
                }
            }
            at += fields + 1;
        }
        // Third pass: move the objects. An object never goes to a higher address, so it
        // overwrites only garbage, or words that have already been moved.
        at = 1;
        while at < self.next {
            let fields = self.kind_at(at).fields();
            self.walked += 1;
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
        self.next = to;
    }

    /// Words for an object of `fields` fields, if there are any.
    fn take(&mut self, fields: usize) -> Option<usize> {
        let at = self.free[fields - 2];
        if at != 0 {
            self.free[fields - 2] = self.words[at + 1] as usize;
            return Some(at);
        }
        if self.next + fields + 1 > self.words.len() {
            return None;
        }
        self.next += fields + 1;
        Some(self.next - fields - 1)
    }
}

impl Collect for MarkHeap {
    fn collect(&mut self) {
        let started = self.clock.map_or(0, |clock| clock());
        self.mark_roots();
        while self.mark_some(usize::MAX) {}
        match self.reclaim {
            Reclaim::Sweep => self.sweep(),
            Reclaim::Compact => self.compact(),
        }
        self.counts.traced += self.traced;
        self.counts.walked += self.walked;
        self.counts.finish(self.traced, self.walked, self.clock, started);
    }

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

impl Heap for MarkHeap {
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
        }
        let at = match self.take(fields) {
            Some(at) => at,
            None => {
                // The fields of the object to be are on the root stack: they are marked,
                // and if the collection moves what they refer to, they are corrected.
                self.collect();
                self.take(fields).ok_or(Full)?
            }
        };
        let first = self.roots.len() - fields;
        for i in 0..fields {
            self.words[at + 1 + i] = self.roots[first + i].bits();
        }
        self.roots.truncate(first);
        self.words[at] = Self::header(kind);
        self.roots.push(Value::obj(Obj(at as u32)));
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
    }

    /// `freed` counts what collections found dead, so `live` and `live_words` include
    /// the objects that have died since the last one.
    fn stats(&self) -> Stats {
        self.stats
    }
}
