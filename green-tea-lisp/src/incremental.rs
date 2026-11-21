//! A heap that marks and sweeps in steps, between the steps of the program.
//!
//! The memory, the header word and the mark phase are those of [`crate::mark::MarkHeap`],
//! copied here and made safe to interleave. A *cycle* marks what the root stack reaches
//! and then sweeps, as a collection of `MarkHeap` does, but it is done a few *units* at a
//! time: a unit is one root slot read, one object taken off the mark stack and its fields
//! read, or one object passed over by the sweep. Between two steps the program runs, and
//! changes the graph that the cycle is half way through.
//!
//! In the three colours of Dijkstra and his colleagues: an object is *white* if it isn't
//! marked, *grey* if it is marked and waits on the mark stack, and *black* if it is marked
//! and its fields have been read. Marking is over when nothing is grey, and the sweep then
//! frees what is white. That is only right if no reachable object is white at that moment,
//! and a program that stores a reference to a white object in a black one, and then loses
//! every other reference to it, makes one. Three things keep it from happening:
//!
//! - The *write barrier* ([`Barrier`]) in `set_field`, the one place where a reference is
//!   stored in an object that exists. [`Barrier::Insertion`] shades the object whose
//!   reference is stored (Dijkstra's). [`Barrier::Deletion`] shades the object whose
//!   reference is overwritten (a snapshot at the beginning: Yuasa's).
//!   [`Barrier::None`] is there to show what goes wrong without one.
//! - The colour an object is born with while a cycle marks ([`Birth`]).
//! - What is done about the root stack, which has no barrier: `push`, `pop` and
//!   `set_root` are plain stores. Every slot is shaded when a cycle starts. With the
//!   insertion barrier the slots are shaded again each time the mark stack runs empty,
//!   and marking ends when that finds nothing new. With the deletion barrier one scan is
//!   enough, because everything reachable when the cycle started gets marked whatever the
//!   program does afterwards.
//!
//! When the steps happen is the fourth choice ([`Pace`]): a fixed number of units at each
//! allocation, a number of units or a length of time at each tick of a timer that `alloc`
//! polls, the whole cycle at once, or only when the caller says. Steps happen inside
//! `alloc` (and `collect`) and nowhere else, so the rule the evaluator already obeys for a
//! heap that traces covers this one too.
//!
//! A cycle starts when `alloc` finds room for fewer than a *reserve* of objects of the size
//! it was asked for, counting the free list for that size and the words never used. The
//! reserve is what the program may allocate while the cycle runs. If `alloc` finds no
//! room at all, it finishes the cycle in progress on the spot, however long that takes,
//! and then runs another: a *stall*, which is the pause this heap exists to avoid.

use crate::gc::{Collect, Counts, reference};
use crate::programs::Program;
use crate::{Full, Heap, Kind, Obj, Stats, Value};
use alloc::vec;
use alloc::vec::Vec;

/// What `set_field` does while a cycle is marking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Barrier {
    /// Nothing. A reachable object can be freed.
    None,
    /// Shade the object whose reference is being stored.
    Insertion,
    /// Shade the object whose reference is being overwritten.
    Deletion,
}

/// The colour of an object allocated while a cycle is marking. Only the insertion
/// barrier has the choice: the other two allocate black.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Birth {
    /// Unmarked. If it dies before marking ends, this cycle frees it; if it lives, some
    /// scan of the root stack has to find it.
    White,
    /// Marked, and what its fields refer to is shaded. It survives this cycle.
    Black,
}

/// When the steps of a cycle are taken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pace {
    /// Only when the caller calls [`IncHeap::start`] and [`IncHeap::step`].
    Manual,
    /// The whole cycle in one step, when the threshold is reached.
    Whole,
    /// This many units at every allocation, while a cycle is in progress.
    Alloc(usize),
    /// This many units at an allocation that finds a tick pending.
    Tick(usize),
    /// At an allocation that finds a tick pending, work until the clock has advanced by
    /// this much.
    Slice(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Mark,
    Sweep,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Colour {
    White,
    Grey,
    Black,
}

/// The buckets of [`Steps::pauses`].
pub const PAUSE_BUCKETS: usize = 24;

/// What an incremental heap counts beside [`Stats`] and [`Counts`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Steps {
    /// Steps taken, the units of work in them, and the most units in one step.
    pub steps: u64,
    pub units: u64,
    pub largest: u64,
    /// Times `alloc` found no room and finished a cycle on the spot, and the most units
    /// one of those took. Not counted in `steps`, `units` or `largest`.
    pub stalls: u64,
    pub largest_stall: u64,
    /// Scans of the root stack, and the most slots one of them read. A cycle starts
    /// with one; `rescans` counts those made because the mark stack had run empty.
    pub scans: u64,
    pub rescans: u64,
    pub most_roots: u64,
    /// Calls of `set_field`, how many of them found a cycle marking, and how many of
    /// those turned a white object grey.
    pub stores: u64,
    pub barriers: u64,
    pub shaded: u64,
    /// Objects allocated marked, during marking or ahead of the sweep.
    pub born_marked: u64,
    /// With [`IncHeap::set_audit`]: objects that were marked when marking ended and that
    /// the root stack couldn't reach (they survive the cycle as garbage), the most of
    /// them in one cycle, and objects that the root stack could reach and that weren't
    /// marked (they are freed while in use).
    pub floating: u64,
    pub largest_floating: u64,
    pub lost: u64,
    /// With a clock: how long the steps took. Bucket 0 counts steps of no ticks at all,
    /// and bucket `n` those of at least `2^(n-1)` ticks and fewer than `2^n`.
    pub pauses: [u32; PAUSE_BUCKETS],
}

// The header word, as in `mark.rs`.
const KIND_MASK: u64 = 0b11;
const MARK: u64 = 1 << 2;
const FREE: u64 = 1 << 3;

pub struct IncHeap {
    words: Vec<u64>,
    /// The first word never yet used. Word 0 is never used, so that 0 can end a list.
    next: usize,
    /// The free lists, one for objects of two fields and one for three, linked through
    /// the first field, and how many objects are on each. The sweep puts an object at
    /// the front.
    free: [usize; 2],
    free_count: [usize; 2],
    roots: Vec<Value>,
    barrier: Barrier,
    birth: Birth,
    pace: Pace,
    phase: Phase,
    /// A cycle starts when there is room for fewer objects than this.
    reserve: usize,
    /// The grey objects.
    stack: Vec<u32>,
    stack_limit: usize,
    /// An object was marked while the stack was full: it is grey, and nothing says so.
    overflowed: bool,
    /// Where the search for such objects has got to, if one is under way.
    search: Option<usize>,
    /// The sweep has dealt with every object below `sweep_at`, and stops at `sweep_end`.
    sweep_at: usize,
    sweep_end: usize,
    stress: bool,
    audit: bool,
    strict: bool,
    clock: Option<fn() -> u64>,
    ticker: Option<fn() -> bool>,
    stats: Stats,
    counts: Counts,
    steps: Steps,
    /// What the cycle in progress has traced and walked.
    traced: u64,
    walked: u64,
}

impl IncHeap {
    /// A heap of `words` words with a mark stack of `mark_stack` entries. Objects born
    /// during marking are white under the insertion barrier ([`IncHeap::born`] changes
    /// that), and the reserve is an object for every eight words of the heap
    /// ([`IncHeap::reserving`]).
    pub fn new(words: usize, barrier: Barrier, pace: Pace, mark_stack: usize) -> IncHeap {
        IncHeap {
            words: vec![0; words + 1],
            next: 1,
            free: [0; 2],
            free_count: [0; 2],
            roots: Vec::with_capacity(1024),
            barrier,
            birth: Birth::White,
            pace,
            phase: Phase::Idle,
            reserve: words / 8,
            stack: Vec::with_capacity(mark_stack),
            stack_limit: mark_stack,
            overflowed: false,
            search: None,
            sweep_at: 1,
            sweep_end: 1,
            stress: false,
            audit: false,
            strict: false,
            clock: None,
            ticker: None,
            stats: Stats::default(),
            counts: Counts::default(),
            steps: Steps::default(),
            traced: 0,
            walked: 0,
        }
    }

    /// The colour of objects allocated during marking, under the insertion barrier.
    pub fn born(mut self, birth: Birth) -> IncHeap {
        self.birth = birth;
        self
    }

    /// Start a cycle when there is room for fewer than `objects` more objects of the size
    /// being allocated.
    pub fn reserving(mut self, objects: usize) -> IncHeap {
        self.reserve = objects;
        self
    }

    /// For [`Pace::Tick`] and [`Pace::Slice`]: `ticker` says whether a tick has happened
    /// since it was last asked.
    pub fn set_ticker(&mut self, ticker: fn() -> bool) {
        self.ticker = Some(ticker);
    }

    /// Check every cycle, when its marking ends, against a walk from the root stack that
    /// takes no account of marks, and count what is found in [`Steps::floating`] and
    /// [`Steps::lost`]. The walk reads every reachable object, and is no part of a step.
    pub fn set_audit(&mut self, audit: bool) {
        self.audit = audit;
    }

    /// With the audit: panic at the end of the first mark phase that lost an object,
    /// before the sweep frees it and the program goes on with a heap that is wrong.
    pub fn set_strict(&mut self, strict: bool) {
        self.strict = strict;
    }

    pub fn steps(&self) -> Steps {
        self.steps
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The colour of an allocated object. Meaningful while a cycle is marking. An object
    /// that didn't fit on the mark stack is reported as black.
    pub fn colour(&self, obj: Obj) -> Colour {
        let at = self.live(obj);
        if self.words[at] & MARK == 0 {
            Colour::White
        } else if self.stack.contains(&obj.0) {
            Colour::Grey
        } else {
            Colour::Black
        }
    }

    /// Whether `obj` is the index of an object that is allocated. A reference to an
    /// object that the sweep freed says no, until its words are allocated again.
    pub fn is_allocated(&self, obj: Obj) -> bool {
        let at = obj.0 as usize;
        at >= 1 && at < self.next && self.words[at] & KIND_MASK != 0 && self.words[at] & FREE == 0
    }

    // ---- headers ----------------------------------------------------------------------

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

    // ---- marking ----------------------------------------------------------------------

    /// *Shade* what `word` refers to: if it is a reference to a white object, make the
    /// object grey. Returns whether it did.
    fn shade(&mut self, word: u64) -> bool {
        let Some(at) = reference(word) else { return false };
        assert!(at >= 1 && at < self.next && self.words[at] & FREE == 0, "a reference to a freed object: {at}");
        if self.words[at] & MARK != 0 {
            return false;
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
        true
    }

    /// Shade what every root slot refers to. A unit for each slot.
    fn shade_roots(&mut self) -> u64 {
        self.counts.roots += self.roots.len() as u64;
        self.steps.scans += 1;
        self.steps.most_roots = self.steps.most_roots.max(self.roots.len() as u64);
        for slot in 0..self.roots.len() {
            self.shade(self.roots[slot].bits());
        }
        self.roots.len() as u64
    }

    /// Start a cycle: every object is white, and the roots are shaded.
    fn begin(&mut self) -> u64 {
        debug_assert!(self.phase == Phase::Idle && self.stack.is_empty());
        self.phase = Phase::Mark;
        self.traced = 0;
        self.walked = 0;
        self.shade_roots()
    }

    /// Make a grey object black: shade what its fields refer to.
    fn scan(&mut self, at: usize) {
        for i in 0..self.kind_at(at).fields() {
            self.shade(self.words[at + 1 + i]);
        }
    }

    /// One object of the search for grey objects that aren't on the mark stack. It goes
    /// through the heap in address order, as `MarkHeap`'s does, but one object to a unit,
    /// and it can be left and taken up again. An object marked without room on the stack
    /// while the search is under way sets `overflowed` again, and the heap is searched
    /// once more after this search ends.
    fn search_one(&mut self) {
        let at = match self.search {
            Some(at) => at,
            None => {
                self.overflowed = false;
                self.counts.rescans += 1;
                1
            }
        };
        if at >= self.next {
            self.search = None;
            return;
        }
        let fields = self.kind_at(at).fields();
        self.walked += 1;
        if self.words[at] & MARK != 0 {
            self.scan(at);
        }
        self.search = Some(at + fields + 1);
    }

    /// Nothing is grey. What is white now is freed by the sweep that starts here.
    fn end_marking(&mut self) {
        if self.audit {
            self.check();
        }
        self.phase = Phase::Sweep;
        self.sweep_at = 1;
        self.sweep_end = self.next;
    }

    /// The audit: walk from the root stack without looking at marks, and compare.
    fn check(&mut self) {
        let mut seen = vec![false; self.next];
        let mut work: Vec<usize> = self.roots.iter().filter_map(|v| reference(v.bits())).collect();
        while let Some(at) = work.pop() {
            if seen[at] {
                continue;
            }
            seen[at] = true;
            if self.words[at] & FREE != 0 {
                continue; // freed by an earlier cycle that lost it
            }
            for i in 0..self.kind_at(at).fields() {
                work.extend(reference(self.words[at + 1 + i]));
            }
        }
        let (mut at, mut floating) = (1, 0);
        while at < self.next {
            let marked = self.words[at] & MARK != 0;
            floating += u64::from(marked && !seen[at]);
            self.steps.lost += u64::from(seen[at] && !marked);
            at += self.kind_at(at).fields() + 1;
        }
        self.steps.floating += floating;
        self.steps.largest_floating = self.steps.largest_floating.max(floating);
        assert!(!self.strict || self.steps.lost == 0, "a cycle is about to free {} objects in use", self.steps.lost);
    }

    // ---- the sweep --------------------------------------------------------------------

    /// The object at `sweep_at`: clear its mark, or free it.
    fn sweep_one(&mut self) {
        let at = self.sweep_at;
        let fields = self.kind_at(at).fields();
        self.walked += 1;
        if self.words[at] & MARK != 0 {
            self.words[at] &= !MARK;
        } else if self.words[at] & FREE == 0 {
            self.stats.freed += 1;
            self.stats.live_words -= fields + 1;
            self.words[at] = self.words[at] & KIND_MASK | FREE;
            self.words[at + 1] = self.free[fields - 2] as u64;
            self.free[fields - 2] = at;
            self.free_count[fields - 2] += 1;
        }
        self.sweep_at = at + fields + 1;
    }

    fn end_cycle(&mut self) {
        self.phase = Phase::Idle;
        self.counts.traced += self.traced;
        self.counts.walked += self.walked;
        self.counts.finish(self.traced, self.walked, None, 0);
    }

    // ---- steps ------------------------------------------------------------------------

    /// Work on the cycle in progress until `budget` units are done or the clock has
    /// advanced by `slice`, or the cycle ends. With `begin`, start a cycle first if none
    /// is in progress. A scan of the root stack is never cut short, so a step can go over
    /// its budget by the number of root slots. Returns the units done.
    fn work(&mut self, budget: u64, slice: Option<u64>, begin: bool) -> u64 {
        let started = self.clock.map(|clock| clock());
        let mut units = 0;
        if begin && self.phase == Phase::Idle {
            units += self.begin();
        }
        while self.phase != Phase::Idle && units < budget {
            if let (Some(slice), Some(clock), Some(started)) = (slice, self.clock, started)
                && clock().wrapping_sub(started) >= slice
            {
                break;
            }
            match self.phase {
                Phase::Mark => {
                    if let Some(at) = self.stack.pop() {
                        self.scan(at as usize);
                        units += 1;
                    } else if self.overflowed || self.search.is_some() {
                        self.search_one();
                        units += 1;
                    } else if self.barrier == Barrier::Insertion && self.rescan(&mut units) {
                        // The root stack held a reference to a white object: there is more to mark.
                    } else {
                        self.end_marking();
                    }
                }
                Phase::Sweep if self.sweep_at < self.sweep_end => {
                    self.sweep_one();
                    units += 1;
                }
                _ => self.end_cycle(),
            }
        }
        if let (Some(clock), Some(started)) = (self.clock, started) {
            let pause = clock().wrapping_sub(started);
            self.counts.pause_total += pause;
            self.counts.pause_longest = self.counts.pause_longest.max(pause);
            let bucket = (u64::BITS - pause.leading_zeros()) as usize;
            self.steps.pauses[bucket.min(PAUSE_BUCKETS - 1)] += 1;
        }
        units
    }

    /// The mark stack is empty, and the root stack has changed since it was scanned.
    /// Scan it again, and say whether that shaded anything.
    fn rescan(&mut self, units: &mut u64) -> bool {
        let before = self.traced;
        self.steps.rescans += 1;
        *units += self.shade_roots();
        self.traced != before
    }

    fn record(&mut self, units: u64) {
        self.steps.steps += 1;
        self.steps.units += units;
        self.steps.largest = self.steps.largest.max(units);
    }

    /// Start a cycle, if none is in progress: one step, of a unit for each root slot.
    pub fn start(&mut self) {
        if self.phase == Phase::Idle {
            let units = self.work(0, None, true);
            self.record(units);
        }
    }

    /// One step of at most `budget` units. Returns whether a cycle is still in progress.
    pub fn step(&mut self, budget: usize) -> bool {
        if self.phase != Phase::Idle {
            let units = self.work(budget as u64, None, false);
            self.record(units);
        }
        self.phase != Phase::Idle
    }

    /// How many more objects of `fields` fields there is room for.
    fn room(&self, fields: usize) -> usize {
        self.free_count[fields - 2] + (self.words.len() - self.next) / (fields + 1)
    }

    /// What `alloc` does before it allocates an object of `fields` fields.
    fn paced(&mut self, fields: usize) {
        // A tick is taken whether or not there is work for it, so that the next one
        // found is a new one.
        let ticked = match self.pace {
            Pace::Tick(_) | Pace::Slice(_) => self.ticker.is_some_and(|pending| pending()),
            _ => true,
        };
        let wanted = self.phase != Phase::Idle || self.stress || self.room(fields) < self.reserve;
        if !wanted || !ticked {
            return;
        }
        let units = match self.pace {
            Pace::Manual => return,
            Pace::Whole => self.work(u64::MAX, None, true),
            Pace::Alloc(units) | Pace::Tick(units) => self.work(units as u64, None, true),
            Pace::Slice(ticks) => self.work(u64::MAX, Some(ticks), true),
        };
        self.record(units);
    }

    /// `alloc` has no room. Finish the cycle in progress; if that frees nothing of the
    /// right size, run a whole cycle more, which frees what the first one left floating.
    fn stall(&mut self, fields: usize) -> Option<usize> {
        self.steps.stalls += 1;
        let mut units = 0;
        if self.phase != Phase::Idle {
            units += self.work(u64::MAX, None, false);
        }
        let mut at = self.take(fields);
        if at.is_none() {
            units += self.work(u64::MAX, None, true);
            at = self.take(fields);
        }
        self.steps.largest_stall = self.steps.largest_stall.max(units);
        at
    }

    /// Words for an object of `fields` fields, if there are any.
    fn take(&mut self, fields: usize) -> Option<usize> {
        let at = self.free[fields - 2];
        if at != 0 {
            self.free[fields - 2] = self.words[at + 1] as usize;
            self.free_count[fields - 2] -= 1;
            return Some(at);
        }
        if self.next + fields + 1 > self.words.len() {
            return None;
        }
        self.next += fields + 1;
        Some(self.next - fields - 1)
    }
}

impl Collect for IncHeap {
    /// Finish the cycle in progress, and then run a whole one, so that what is allocated
    /// afterwards is what the root stack reaches.
    fn collect(&mut self) {
        let mut units = 0;
        if self.phase != Phase::Idle {
            units += self.work(u64::MAX, None, false);
        }
        units += self.work(u64::MAX, None, true);
        self.record(units);
    }

    fn counts(&self) -> Counts {
        self.counts
    }

    fn capacity(&self) -> usize {
        self.words.len() - 1
    }

    /// Start a cycle at every allocation that finds none in progress, whatever the
    /// threshold: the program then runs with a cycle under way all the time.
    fn set_stress(&mut self, stress: bool) {
        self.stress = stress;
    }

    /// Time every step with `clock`.
    fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = Some(clock);
    }
}

impl Heap for IncHeap {
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
        self.paced(fields);
        let at = match self.take(fields) {
            Some(at) => at,
            None => self.stall(fields).ok_or(Full)?,
        };
        let first = self.roots.len() - fields;
        for i in 0..fields {
            self.words[at + 1 + i] = self.roots[first + i].bits();
        }
        // The colour the object is born with. While a cycle marks, that is the choice of
        // `Birth`. While it sweeps, an object that the sweep hasn't reached must be
        // marked, or the sweep frees it; the sweep clears the mark when it gets there.
        let marked = match self.phase {
            Phase::Idle => false,
            Phase::Mark => self.barrier != Barrier::Insertion || self.birth == Birth::Black,
            Phase::Sweep => at >= self.sweep_at && at < self.sweep_end,
        };
        self.words[at] = Self::header(kind) | if marked { MARK } else { 0 };
        self.steps.born_marked += u64::from(marked);
        if marked && self.phase == Phase::Mark && self.barrier == Barrier::Insertion {
            // A black object whose fields came off the root stack, which nothing has
            // scanned since they were pushed.
            self.scan(at);
        }
        self.roots.truncate(first);
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
        self.steps.stores += 1;
        if self.phase == Phase::Mark {
            // The write barrier.
            self.steps.barriers += 1;
            let shaded = match self.barrier {
                Barrier::None => false,
                Barrier::Insertion => self.shade(v.bits()),
                Barrier::Deletion => self.shade(self.words[at + 1 + i]),
            };
            self.steps.shaded += u64::from(shaded);
        }
        self.words[at + 1 + i] = v.bits();
    }

    /// `freed` counts what the sweeps have freed, so `live` and `live_words` include the
    /// objects that have died and that no sweep has reached.
    fn stats(&self) -> Stats {
        self.stats
    }
}

/// Two programs that change objects that exist, which the five of [`crate::programs`]
/// hardly do: a write barrier has nothing to see in a program that only allocates.
pub const MUTATORS: [Program; 2] = [
    Program { name: "nreverse", source: include_str!("../programs/nreverse.lisp"), value: "20100", cyclic: false },
    Program { name: "queue", source: include_str!("../programs/queue.lisp"), value: "125250", cyclic: false },
];

#[cfg(test)]
#[path = "incremental_tests.rs"]
mod tests;
