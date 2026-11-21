//! A heap that counts references.
//!
//! The heap is an array of words. An object is a header word followed by its fields, and
//! a reference to it is the index of its header. The header holds the object's kind and
//! its *count*: how many root slots and fields refer to it. `push`, `set_root` and
//! `set_field` raise the count of what they store; `pop`, `set_root` and `set_field` lower
//! the count of what they drop; an object whose count reaches zero is freed, and so each
//! of its fields is dropped in turn.
//!
//! How that last step is done is a choice ([`Freeing`]): by recursion, from a queue
//! threaded through the dead objects, or a few objects at a time as later allocations
//! make room. Freed objects go on a free list for their size, and `alloc` takes from it
//! before it takes fresh words.
//!
//! Counts can't free a cycle. [`Cycles::Collect`] adds a cycle collector by trial deletion
//! ([`RcHeap::collect_cycles`]).
//!
//! Everything is counted ([`Counts`]), and nothing is timed.

use crate::{Full, Heap, Kind, Obj, Stats, Value};
use alloc::vec;
use alloc::vec::Vec;

/// What to do with an object whose count has reached zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Freeing {
    /// Drop each of its fields, by a call that may free in turn, and then free it. The
    /// stack grows with the longest chain of objects that die together.
    Recursive,
    /// Put it on a list of dead objects, linked through their headers, and empty the list
    /// before the operation that started it returns. Constant stack, and the same work.
    Queued,
    /// Put it on the list and return. Each `alloc` takes [`LAZY_STEP`] objects off the
    /// list, and drops their fields then. Constant stack and bounded work, and memory
    /// that is free later than it could be.
    Lazy,
}

/// How many dead objects a lazy heap finishes freeing at each allocation.
pub const LAZY_STEP: usize = 2;

/// What to do about cycles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cycles {
    /// Nothing: a cycle that nothing refers to stays allocated.
    Leak,
    /// Remember every object whose count was lowered to something above zero, and look
    /// for cycles through them when `every` are remembered or the heap is full.
    Collect { every: usize },
}

/// Why a count changed: for a root slot, or for a field.
#[derive(Clone, Copy)]
enum Slot {
    Root,
    Field,
}

/// What a reference-counting heap counts beside [`Stats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Increments for a value put in a root slot, and decrements for one taken out.
    pub root_increments: u64,
    pub root_decrements: u64,
    /// The same, for fields. A field's decrement may be the freeing of its object.
    pub field_increments: u64,
    pub field_decrements: u64,
    /// The most objects freed by one operation of the interpreter.
    pub largest_burst: u64,
    /// [`Freeing::Recursive`]: the deepest the freeing recursed, in objects, and how many
    /// bytes of stack that took. The bytes depend on the compiler and the architecture.
    pub deepest: usize,
    pub deepest_bytes: usize,
    /// [`Cycles::Collect`]: objects remembered as candidates, searches made, the counts
    /// lowered on trial during them, and the objects they freed.
    pub candidates: u64,
    pub searches: u64,
    pub trial_decrements: u64,
    pub cycle_freed: u64,
}

impl Counts {
    pub fn increments(&self) -> u64 {
        self.root_increments + self.field_increments
    }

    pub fn decrements(&self) -> u64 {
        self.root_decrements + self.field_decrements
    }
}

// The header word. The kind is never cleared, so the heap can be walked object by object
// whether or not they are allocated.
const KIND_MASK: u64 = 0b11;
const COLOR_SHIFT: u32 = 2;
const COLOR_MASK: u64 = 0b11 << COLOR_SHIFT;
/// The cycle collector's candidate list names this object.
const BUFFERED: u64 = 1 << 4;
/// The count reached zero: the object is waiting to be freed, or to leave the candidate list.
const DEAD: u64 = 1 << 5;
/// On a free list.
const FREE: u64 = 1 << 6;
/// The count is the top half of the header. A dead object's is its link in the queue.
const COUNT_SHIFT: u32 = 32;

/// The cycle collector's colours. The names are Bacon and Rajan's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Color {
    /// In use, or free.
    Black = 0,
    /// Being searched: its count has had the references from inside the search removed.
    Gray = 1,
    /// Found to be held only from inside the search: garbage, unless a later step says otherwise.
    White = 2,
    /// A candidate: its count was lowered, and didn't reach zero.
    Purple = 3,
}

pub struct RcHeap {
    words: Vec<u64>,
    /// The first word never yet used. Word 0 is never used, so that 0 can end a list.
    next: usize,
    /// The free lists, one for objects of two fields and one for three, linked through
    /// the first field.
    free: [usize; 2],
    /// The queue of dead objects, linked through their counts.
    dead: usize,
    roots: Vec<Value>,
    freeing: Freeing,
    cycles: Cycles,
    candidates: Vec<Obj>,
    work: Vec<Obj>,
    stats: Stats,
    counts: Counts,
    depth: usize,
    stack_top: usize,
    burst: u64,
}

impl RcHeap {
    /// A heap of `words` words (8 bytes each), all of them unused.
    pub fn new(words: usize, freeing: Freeing, cycles: Cycles) -> RcHeap {
        RcHeap {
            words: vec![0; words + 1],
            next: 1,
            free: [0; 2],
            dead: 0,
            roots: Vec::with_capacity(1024),
            freeing,
            cycles,
            candidates: Vec::new(),
            work: Vec::new(),
            stats: Stats::default(),
            counts: Counts::default(),
            depth: 0,
            stack_top: 0,
            burst: 0,
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The count in an object's header.
    pub fn count(&self, obj: Obj) -> u32 {
        (self.words[self.live(obj)] >> COUNT_SHIFT) as u32
    }

    /// How many words the heap has, and how many have ever been used.
    pub fn capacity(&self) -> usize {
        self.words.len() - 1
    }

    pub fn touched(&self) -> usize {
        self.next - 1
    }

    /// Free everything that is dead and still queued. A lazy heap needs this before its
    /// counters say what is garbage; the others have nothing queued between operations.
    pub fn finish_freeing(&mut self) {
        self.burst = 0;
        self.drain(usize::MAX);
    }

    // ---- headers ----------------------------------------------------------------------

    /// The index of a live object's header.
    fn live(&self, obj: Obj) -> usize {
        let at = obj.0 as usize;
        assert!(at >= 1 && at < self.next, "not an object: {at}");
        assert!(self.words[at] & (FREE | DEAD) == 0, "a freed object was used: {at}");
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

    fn header(kind: Kind, count: u32) -> u64 {
        let kind = match kind {
            Kind::Pair => 1,
            Kind::Closure => 2,
            Kind::Binding => 3,
        };
        u64::from(count) << COUNT_SHIFT | kind
    }

    fn set_count(&mut self, at: usize, count: u32) {
        self.words[at] = self.words[at] & ((1 << COUNT_SHIFT) - 1) | u64::from(count) << COUNT_SHIFT;
    }

    fn color(&self, at: usize) -> Color {
        match (self.words[at] & COLOR_MASK) >> COLOR_SHIFT {
            0 => Color::Black,
            1 => Color::Gray,
            2 => Color::White,
            _ => Color::Purple,
        }
    }

    fn set_color(&mut self, at: usize, color: Color) {
        self.words[at] = self.words[at] & !COLOR_MASK | (color as u64) << COLOR_SHIFT;
    }

    /// The object a field refers to, if it refers to one.
    fn child(&self, at: usize, i: usize) -> Option<usize> {
        Value::from_bits(self.words[at + 1 + i]).as_obj().map(|o| o.0 as usize)
    }

    // ---- counting ---------------------------------------------------------------------

    fn increment(&mut self, v: Value, slot: Slot) {
        let Some(obj) = v.as_obj() else { return };
        let at = self.live(obj);
        match slot {
            Slot::Root => self.counts.root_increments += 1,
            Slot::Field => self.counts.field_increments += 1,
        }
        let count = (self.words[at] >> COUNT_SHIFT) as u32;
        self.set_count(at, count.checked_add(1).expect("a count overflowed"));
        if self.cycles != Cycles::Leak {
            self.set_color(at, Color::Black); // referred to again: not a candidate
        }
    }

    fn decrement(&mut self, v: Value, slot: Slot) {
        let Some(obj) = v.as_obj() else { return };
        let at = self.live(obj);
        match slot {
            Slot::Root => self.counts.root_decrements += 1,
            Slot::Field => self.counts.field_decrements += 1,
        }
        let count = (self.words[at] >> COUNT_SHIFT) as u32 - 1;
        self.set_count(at, count);
        if count == 0 {
            self.words[at] |= DEAD;
            match self.freeing {
                Freeing::Recursive => self.free_recursively(at),
                Freeing::Queued | Freeing::Lazy => {
                    // A dead object needs no count: the queue's link goes there.
                    self.set_count(at, self.dead as u32);
                    self.dead = at;
                }
            }
        } else if self.cycles != Cycles::Leak {
            self.candidate(at);
        }
    }

    /// A decrement made by the interpreter, and all it leads to.
    fn release(&mut self, v: Value, slot: Slot) {
        self.burst = 0;
        self.decrement(v, slot);
        if self.freeing == Freeing::Queued {
            self.drain(usize::MAX);
        }
        self.counts.largest_burst = self.counts.largest_burst.max(self.burst);
    }

    fn free_recursively(&mut self, at: usize) {
        let marker = 0u8;
        let here = core::hint::black_box(&raw const marker) as usize;
        if self.depth == 0 {
            self.stack_top = here;
        }
        self.depth += 1;
        if self.depth > self.counts.deepest {
            self.counts.deepest = self.depth;
            self.counts.deepest_bytes = self.stack_top.abs_diff(here);
        }
        for i in 0..self.kind_at(at).fields() {
            let field = Value::from_bits(self.words[at + 1 + i]);
            self.decrement(field, Slot::Field); // which may call this function
        }
        self.reclaim(at);
        self.depth -= 1;
    }

    /// Take up to `limit` objects off the queue of the dead, drop their fields, and free them.
    fn drain(&mut self, limit: usize) {
        let mut done = 0;
        while self.dead != 0 && done < limit {
            let at = self.dead;
            self.dead = (self.words[at] >> COUNT_SHIFT) as usize;
            self.set_count(at, 0);
            for i in 0..self.kind_at(at).fields() {
                let field = Value::from_bits(self.words[at + 1 + i]);
                self.decrement(field, Slot::Field); // may put more on the queue
            }
            self.reclaim(at);
            done += 1;
        }
        self.counts.largest_burst = self.counts.largest_burst.max(self.burst);
    }

    /// The object at `at` is dead and its fields have been dropped.
    fn reclaim(&mut self, at: usize) {
        let words = self.kind_at(at).fields() + 1;
        self.stats.freed += 1;
        self.stats.live_words -= words;
        self.burst += 1;
        if self.words[at] & BUFFERED == 0 {
            self.put_on_free_list(at);
        }
        // Otherwise the candidate list still names it, and it stays as it is, dead, until
        // the next search goes through the list.
    }

    fn put_on_free_list(&mut self, at: usize) {
        let list = self.kind_at(at).fields() - 2;
        self.words[at] = self.words[at] & KIND_MASK | FREE;
        self.words[at + 1] = self.free[list] as u64;
        self.free[list] = at;
    }

    /// Words for an object of `fields` fields: from the free list, or never used.
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

    // ---- cycles -----------------------------------------------------------------------
    //
    // Trial deletion, in the form credited to Bacon and Rajan. A cycle of garbage
    // comes into being when a count is lowered and doesn't reach zero, so those objects
    // are the candidates. From each, take away the references that come from objects
    // reachable from it. Whatever is then at zero was held only from inside; whatever is
    // above zero is held from outside, and it and all it reaches get their counts back.

    fn candidate(&mut self, at: usize) {
        self.set_color(at, Color::Purple);
        if self.words[at] & BUFFERED == 0 {
            self.words[at] |= BUFFERED;
            self.candidates.push(Obj(at as u32));
            self.counts.candidates += 1;
        }
    }

    /// Search the candidates for cycles that nothing outside them refers to, and free
    /// them. Returns how many objects it freed. Only between the interpreter's operations.
    pub fn collect_cycles(&mut self) -> u64 {
        self.drain(usize::MAX);
        self.counts.searches += 1;
        let candidates = core::mem::take(&mut self.candidates);
        let mut kept = Vec::new();
        for &Obj(at) in &candidates {
            let at = at as usize;
            if self.words[at] & DEAD != 0 {
                self.put_on_free_list(at); // freed since it became a candidate
            } else if self.color(at) == Color::Purple {
                self.mark_gray(at);
                kept.push(at);
            } else {
                self.words[at] &= !BUFFERED; // referred to again since, or reached from another candidate
            }
        }
        for &at in &kept {
            self.scan(at);
        }
        let before = self.stats.freed;
        for &at in &kept {
            self.words[at] &= !BUFFERED;
            self.collect_white(at);
        }
        let freed = self.stats.freed - before;
        self.counts.cycle_freed += freed;
        freed
    }

    /// Colour gray everything reachable from `at`, taking one from the count of an object
    /// for each reference to it from a gray object.
    fn mark_gray(&mut self, at: usize) {
        if self.color(at) == Color::Gray {
            return;
        }
        self.set_color(at, Color::Gray);
        self.work.push(Obj(at as u32));
        while let Some(Obj(at)) = self.work.pop() {
            let at = at as usize;
            for i in 0..self.kind_at(at).fields() {
                let Some(child) = self.child(at, i) else { continue };
                let count = (self.words[child] >> COUNT_SHIFT) as u32;
                self.set_count(child, count - 1);
                self.counts.trial_decrements += 1;
                if self.color(child) != Color::Gray {
                    self.set_color(child, Color::Gray);
                    self.work.push(Obj(child as u32));
                }
            }
        }
    }

    /// A gray object whose count is still above zero is held from outside: it is live,
    /// and so is everything it reaches. The rest of the gray objects become white.
    fn scan(&mut self, at: usize) {
        let mut work = core::mem::take(&mut self.work);
        work.push(Obj(at as u32));
        while let Some(Obj(at)) = work.pop() {
            let at = at as usize;
            if self.color(at) != Color::Gray {
                continue;
            }
            if self.words[at] >> COUNT_SHIFT > 0 {
                self.scan_black(at);
            } else {
                self.set_color(at, Color::White);
                for i in 0..self.kind_at(at).fields() {
                    if let Some(child) = self.child(at, i) {
                        work.push(Obj(child as u32));
                    }
                }
            }
        }
        self.work = work;
    }

    /// Give back what `mark_gray` took, to everything reachable from a live object.
    fn scan_black(&mut self, at: usize) {
        let mut work = Vec::new();
        self.set_color(at, Color::Black);
        work.push(at);
        while let Some(at) = work.pop() {
            for i in 0..self.kind_at(at).fields() {
                let Some(child) = self.child(at, i) else { continue };
                let count = (self.words[child] >> COUNT_SHIFT) as u32;
                self.set_count(child, count + 1);
                if self.color(child) != Color::Black {
                    self.set_color(child, Color::Black);
                    work.push(child);
                }
            }
        }
    }

    /// Free the white objects reachable from `at`. One that is still a candidate is left
    /// for its own turn.
    fn collect_white(&mut self, at: usize) {
        let mut work = core::mem::take(&mut self.work);
        work.push(Obj(at as u32));
        while let Some(Obj(at)) = work.pop() {
            let at = at as usize;
            if self.color(at) != Color::White || self.words[at] & (BUFFERED | FREE) != 0 {
                continue;
            }
            for i in 0..self.kind_at(at).fields() {
                if let Some(child) = self.child(at, i) {
                    work.push(Obj(child as u32));
                }
            }
            self.stats.freed += 1;
            self.stats.live_words -= self.kind_at(at).fields() + 1;
            self.put_on_free_list(at);
        }
        self.work = work;
    }

    // ---- checking ---------------------------------------------------------------------

    /// Count every reference again, from the root stack and from the fields of every
    /// object that isn't freed, and compare with the headers.
    pub fn audit(&self) -> Result<(), &'static str> {
        let mut expected = vec![0u32; self.next];
        let mut count = |v: Value| -> Result<(), &'static str> {
            if let Some(Obj(at)) = v.as_obj() {
                let at = at as usize;
                if at == 0 || at >= self.next || self.words[at] & (FREE | DEAD) != 0 {
                    return Err("a reference to an object that was freed");
                }
                expected[at] += 1;
            }
            Ok(())
        };
        for &root in &self.roots {
            count(root)?;
        }
        let (mut at, mut live, mut live_words) = (1, 0, 0);
        while at < self.next {
            let fields = self.kind_at(at).fields();
            if self.words[at] & (FREE | DEAD) == 0 {
                live += 1;
                live_words += fields + 1;
                for i in 0..fields {
                    count(Value::from_bits(self.words[at + 1 + i]))?;
                }
            }
            at += fields + 1;
        }
        if self.dead != 0 {
            return Err("dead objects are still queued");
        }
        if live != self.stats.live() || live_words != self.stats.live_words {
            return Err("the heap's counters don't match its objects");
        }
        at = 1;
        while at < self.next {
            if self.words[at] & (FREE | DEAD) == 0 && (self.words[at] >> COUNT_SHIFT) as u32 != expected[at] {
                return Err("a count isn't the number of references to its object");
            }
            at += self.kind_at(at).fields() + 1;
        }
        Ok(())
    }

    /// Whether each object that isn't freed can be reached from the root stack: `(reachable,
    /// unreachable)`. A tracing collector's question, asked here to count what leaked.
    pub fn reachable(&self) -> (u64, u64) {
        let mut seen = vec![false; self.next];
        let mut work: Vec<usize> = self.roots.iter().filter_map(|v| v.as_obj()).map(|o| o.0 as usize).collect();
        let mut reachable = 0;
        while let Some(at) = work.pop() {
            if core::mem::replace(&mut seen[at], true) {
                continue;
            }
            reachable += 1;
            for i in 0..self.kind_at(at).fields() {
                work.extend(self.child(at, i));
            }
        }
        (reachable, self.stats.live() - reachable)
    }
}

impl Heap for RcHeap {
    fn push(&mut self, v: Value) {
        self.increment(v, Slot::Root);
        self.roots.push(v);
    }

    fn pop(&mut self) {
        let v = self.roots.pop().expect("pop from an empty root stack");
        self.release(v, Slot::Root);
    }

    fn roots(&self) -> usize {
        self.roots.len()
    }

    fn root(&self, slot: usize) -> Value {
        self.roots[slot]
    }

    fn set_root(&mut self, slot: usize, v: Value) {
        let old = self.roots[slot];
        self.increment(v, Slot::Root); // first: `old` may be all that holds `v`
        self.roots[slot] = v;
        self.release(old, Slot::Root);
    }

    fn alloc(&mut self, kind: Kind) -> Result<(), Full> {
        let fields = kind.fields();
        assert!(self.roots.len() >= fields, "alloc without its fields on the root stack");
        if self.freeing == Freeing::Lazy {
            self.burst = 0;
            self.drain(LAZY_STEP);
        }
        if let Cycles::Collect { every } = self.cycles
            && self.candidates.len() >= every
        {
            self.collect_cycles();
        }
        let at = match self.take(fields) {
            Some(at) => at,
            None => {
                // Out of room. Anything still queued is room; so is a cycle of garbage.
                self.finish_freeing();
                if self.free[fields - 2] == 0 && self.cycles != Cycles::Leak {
                    self.collect_cycles();
                }
                self.take(fields).ok_or(Full)?
            }
        };
        // The fields move from the root stack into the object, and the object's one
        // reference is the root slot it is pushed to: no count changes.
        let first = self.roots.len() - fields;
        for i in 0..fields {
            self.words[at + 1 + i] = self.roots[first + i].bits();
        }
        self.roots.truncate(first);
        self.words[at] = Self::header(kind, 1);
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
        let old = Value::from_bits(self.words[at + 1 + i]);
        self.increment(v, Slot::Field); // first: `old` may be all that holds `v`
        self.words[at + 1 + i] = v.bits();
        self.release(old, Slot::Field);
    }

    fn stats(&self) -> Stats {
        self.stats
    }
}
