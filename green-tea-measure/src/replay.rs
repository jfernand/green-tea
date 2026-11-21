//! A trace, made ready to replay, and the three ways of replaying it: for bytes, for
//! nothing but the allocator's own work, and against a clock.
//!
//! The replay of green-tea-fit keeps a map from each recorded address to the address the
//! new allocator returned. A map is a search at every event, and here the replay itself
//! is being timed, so the map is built once, before any clock is read: [`Ops::new`] gives
//! every block a *slot*, an index into a table, and a slot is reused when its block is
//! freed. Replaying an event is then one read or one write of the table.

use crate::subjects::Subject;
use green_tea_fit::trace::Event;
use std::collections::BTreeMap;
use std::time::Instant;

/// One event of a trace: an allocation of `size` bytes into `slot`, or, if `size` is 0,
/// a free of the block in `slot`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Op {
    pub slot: u32,
    pub size: u32,
    pub align: u32,
}

/// A trace as a list of [`Op`]s.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ops {
    pub ops: Vec<Op>,
    /// How many slots the table needs: the most blocks the trace has at once.
    pub slots: usize,
    pub allocations: usize,
    pub frees: usize,
    /// Frees of blocks the trace never allocated, which are left out.
    pub unmatched: usize,
    /// The bytes allocated in all, and the most that were allocated at once.
    pub total: usize,
    pub peak_live: usize,
}

impl Ops {
    /// # Panics
    /// If the trace allocates at an address that is still allocated: its records are out
    /// of order, and no replay of it means anything.
    pub fn new(events: &[Event]) -> Ops {
        let mut out = Ops::default();
        let mut slot_of = BTreeMap::new();
        let mut unused = Vec::new();
        let mut live = 0;
        for event in events {
            if event.is_free() {
                let Some((slot, size)) = slot_of.remove(&event.addr) else {
                    out.unmatched += 1;
                    continue;
                };
                unused.push(slot);
                live -= size;
                out.frees += 1;
                out.ops.push(Op { slot, size: 0, align: 0 });
            } else {
                let slot = unused.pop().unwrap_or_else(|| {
                    out.slots += 1;
                    out.slots as u32 - 1
                });
                let size = event.size as usize;
                assert!(slot_of.insert(event.addr, (slot, size)).is_none(), "an address was allocated twice");
                live += size;
                out.total += size;
                out.peak_live = out.peak_live.max(live);
                out.allocations += 1;
                out.ops.push(Op { slot, size: event.size, align: event.align });
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// What a slot holds: the address the subject returned, and the size and alignment asked for.
#[derive(Clone, Copy, Default)]
struct Held {
    addr: usize,
    size: u32,
    align: u32,
}

/// The table of a replay. It is made once and used for every pass, so that a timed pass
/// doesn't write to memory that the operating system hasn't mapped yet.
pub struct Table(Vec<Held>);

impl Table {
    pub fn new(ops: &Ops) -> Table {
        Table(vec![Held::default(); ops.slots])
    }
}

/// One event against `subject`. `false` if the subject refused an allocation.
#[inline(always)]
fn step(op: Op, table: &mut Table, subject: &mut dyn Subject) -> bool {
    let table = &mut table.0;
    if op.size == 0 {
        let held = table[op.slot as usize];
        // SAFETY: the slot holds a block that `subject` returned and that hasn't been freed:
        // `Ops::new` matched every free with one allocation.
        unsafe { subject.free(held.addr, held.size as usize, held.align as usize) };
        true
    } else {
        match subject.allocate(op.size as usize, op.align as usize) {
            Some(addr) => {
                table[op.slot as usize] = Held { addr, size: op.size, align: op.align };
                true
            }
            None => false,
        }
    }
}

/// What a trace needed of a subject, in bytes and in searches.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Space {
    /// Allocations the subject refused. Anything but 0 and the other numbers mean nothing.
    pub refused: usize,
    pub peak_live: usize,
    /// The most bytes in allocated blocks, for the subjects that count them.
    pub peak_used: Option<usize>,
    pub footprint: usize,
    /// Free blocks looked at, and the most in one call, for the subjects that search.
    pub searched: Option<(usize, usize)>,
}

/// Replay `ops` against `subject`, which should be new, reading its counters after every
/// event. The subject is checked at the end.
pub fn space(ops: &Ops, subject: &mut dyn Subject) -> Result<Space, &'static str> {
    let mut out = Space { peak_live: ops.peak_live, ..Space::default() };
    let mut table = Table::new(ops);
    for &op in &ops.ops {
        if !step(op, &mut table, subject) {
            out.refused += 1;
            // The block doesn't exist: its free must not reach the subject.
            return Ok(out);
        }
        if op.size != 0 {
            out.peak_used = subject.used().map(|used| used.max(out.peak_used.unwrap_or(0)));
            out.footprint = out.footprint.max(subject.footprint());
        }
    }
    out.searched = subject.searched();
    subject.check()?;
    Ok(out)
}

/// Replay `ops` against `subject` `passes` times, and do nothing else: no clock, no
/// counters. This is the loop whose instructions are counted. `false` if the subject
/// refused an allocation.
#[inline(never)]
pub fn plain(ops: &Ops, table: &mut Table, subject: &mut dyn Subject, passes: usize) -> bool {
    for _ in 0..passes {
        subject.reset();
        for &op in &ops.ops {
            if !step(op, table, subject) {
                return false;
            }
        }
    }
    true
}

/// Write to the block that `op` allocated, as the program that asked for it would: its
/// first byte, a byte in every page of it after that, and its last byte.
#[inline(always)]
fn touch(op: Op, table: &Table) {
    let held = table.0[op.slot as usize];
    let end = held.addr + held.size as usize;
    let mut at = held.addr;
    while at < end {
        // SAFETY: the subject handed out `size` bytes at `addr`, and they are the replay's
        // until it frees them.
        unsafe { (at as *mut u8).write_volatile(0xa5) };
        at += 4096;
    }
    // SAFETY: as above; a block has at least one byte.
    unsafe { ((end - 1) as *mut u8).write_volatile(0xa5) };
}

/// Replay `ops` once against `subject`, which should be new or reset, and return the
/// nanoseconds the whole pass took, read from the clock twice. With `touching`, every
/// block is written to when it is allocated.
pub fn pass(ops: &Ops, table: &mut Table, subject: &mut dyn Subject, touching: bool) -> Option<u64> {
    let start = Instant::now();
    for &op in &ops.ops {
        if !step(op, table, subject) {
            return None;
        }
        if touching && op.size != 0 {
            touch(op, table);
        }
    }
    Some(start.elapsed().as_nanos() as u64)
}

/// Replay `ops` once against `subject`, reading the clock around every event, and push
/// the nanoseconds each took onto `samples`. With `touching`, every block is written to
/// when it is allocated, inside the time of its allocation.
pub fn each(ops: &Ops, table: &mut Table, subject: &mut dyn Subject, samples: &mut Vec<u32>, touching: bool) -> bool {
    for &op in &ops.ops {
        let start = Instant::now();
        let done = step(op, table, subject);
        if done && touching && op.size != 0 {
            touch(op, table);
        }
        samples.push(start.elapsed().as_nanos().min(u32::MAX as u128) as u32);
        if !done {
            return false;
        }
    }
    true
}

/// What reading the clock twice with nothing between costs, as [`each`] measures it:
/// `count` samples.
pub fn clock(count: usize) -> Vec<u32> {
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        samples.push(start.elapsed().as_nanos().min(u32::MAX as u128) as u32);
    }
    samples
}
