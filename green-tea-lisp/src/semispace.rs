//! A heap that copies.
//!
//! The heap's words are two halves, and one of them is in use at a time. An object is a
//! header word followed by its fields, a reference to it is the index of its header, and
//! `alloc` takes the next unused words of the half in use: there is no free list.
//!
//! When that half is full, a collection copies every object that can be reached from the
//! root stack into the other half, one after the other from its start, and the halves
//! change roles. The copy of an object is at another index, so every reference to it is
//! corrected, in the root stack and in the fields of the copies. Nothing is done about
//! the objects that weren't reached: their half is abandoned as it is.
//!
//! The traversal is C. J. Cheney's. An object that has been copied has, in its old
//! header, the index of its copy: a *forwarding pointer*. The copies themselves are the
//! list of objects whose fields are still to be read. `scan` goes through them from the
//! first, `next` is where the next copy goes, and the collection is over when `scan`
//! reaches `next`. No stack and no queue is kept beside the heap.

use crate::gc::{Collect, Counts, reference};
use crate::{Full, Heap, Kind, Obj, Stats, Value};
use alloc::vec;
use alloc::vec::Vec;

// The header word: the kind, which is never 0 in an object, or a forwarding pointer.
const KIND_MASK: u64 = 0b11;
/// Copied: the top half of the header is the index of the copy.
const FORWARDED: u64 = 1 << 2;
const FORWARD_SHIFT: u32 = 32;

pub struct Semispace {
    words: Vec<u64>,
    /// The words in each half.
    half: usize,
    /// Where the half in use starts: 0 or `half`.
    from: usize,
    /// The first unused word of the half in use.
    free: usize,
    roots: Vec<Value>,
    /// Objects in the half in use, reachable or not.
    objects: u64,
    stress: bool,
    clock: Option<fn() -> u64>,
    stats: Stats,
    counts: Counts,
}

impl Semispace {
    /// A heap of `words` words (8 bytes each) all told: two halves of `words / 2`.
    pub fn new(words: usize) -> Semispace {
        Semispace {
            words: vec![0; words / 2 * 2],
            half: words / 2,
            from: 0,
            free: 0,
            roots: Vec::with_capacity(1024),
            objects: 0,
            stress: false,
            clock: None,
            stats: Stats::default(),
            counts: Counts::default(),
        }
    }

    /// The words in use in the current half.
    pub fn used(&self) -> usize {
        self.free - self.from
    }

    /// The index of an object's header, if `obj` is in the half in use.
    fn live(&self, obj: Obj) -> usize {
        let at = obj.0 as usize;
        assert!(at >= self.from && at < self.free, "a stale reference: {at} is not in the half in use");
        assert!(self.words[at] & FORWARDED == 0, "a stale reference: {at} was copied");
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

    /// The word to store in place of `word`. If `word` is a reference, that is a
    /// reference to the copy of its object, which is made now, at `*next`, if there
    /// isn't one yet.
    fn forward(&mut self, word: u64, next: &mut usize) -> u64 {
        let Some(at) = reference(word) else { return word };
        assert!(at >= self.from && at < self.free, "a reference out of the half in use: {at}");
        let header = self.words[at];
        if header & FORWARDED != 0 {
            return Value::obj(Obj((header >> FORWARD_SHIFT) as u32)).bits();
        }
        let size = self.kind_at(at).fields() + 1;
        let copy = *next;
        self.words.copy_within(at..at + size, copy);
        self.words[at] = FORWARDED | (copy as u64) << FORWARD_SHIFT;
        *next += size;
        self.counts.moved_words += size as u64;
        Value::obj(Obj(copy as u32)).bits()
    }
}

impl Collect for Semispace {
    fn collect(&mut self) {
        let started = self.clock.map_or(0, |clock| clock());
        let to = if self.from == 0 { self.half } else { 0 };
        let mut next = to;
        self.counts.roots += self.roots.len() as u64;
        for slot in 0..self.roots.len() {
            self.roots[slot] = Value::from_bits(self.forward(self.roots[slot].bits(), &mut next));
        }
        // The copies between `scan` and `next` still hold references to the old half.
        let (mut scan, mut copied) = (to, 0);
        while scan < next {
            let fields = self.kind_at(scan).fields();
            for i in 0..fields {
                self.words[scan + 1 + i] = self.forward(self.words[scan + 1 + i], &mut next);
            }
            scan += fields + 1;
            copied += 1;
        }
        self.stats.freed += self.objects - copied;
        self.stats.live_words = next - to;
        self.objects = copied;
        self.from = to;
        self.free = next;
        self.counts.traced += copied;
        self.counts.traced_words += (next - to) as u64;
        self.counts.finish(copied, 0, self.clock, started);
    }

    fn counts(&self) -> Counts {
        self.counts
    }

    fn capacity(&self) -> usize {
        self.words.len()
    }

    fn set_stress(&mut self, stress: bool) {
        self.stress = stress;
    }

    fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = Some(clock);
    }
}

impl Heap for Semispace {
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
        if self.stress || self.free + fields + 1 > self.from + self.half {
            // The fields of the object to be are on the root stack, so the collection
            // corrects them like any other root.
            self.collect();
            if self.free + fields + 1 > self.from + self.half {
                return Err(Full);
            }
        }
        let at = self.free;
        self.free += fields + 1;
        let first = self.roots.len() - fields;
        for i in 0..fields {
            self.words[at + 1 + i] = self.roots[first + i].bits();
        }
        self.roots.truncate(first);
        self.words[at] = Self::header(kind);
        self.roots.push(Value::obj(Obj(at as u32)));
        self.objects += 1;
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

    /// `freed` counts what collections left behind, so `live` and `live_words` include
    /// the objects that have died since the last one.
    fn stats(&self) -> Stats {
        self.stats
    }
}
