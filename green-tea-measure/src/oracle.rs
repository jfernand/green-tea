//! Explicit freeing for a program that has no `free` in it, after Hertz and Berger's
//! oracular memory manager.
//!
//! The Lisp of green-tea-lisp never frees an object: its heaps find out which are dead.
//! To ask what the same program would cost with `malloc` and `free`, something has to say
//! where the calls to `free` would go. Two runs do it.
//!
//! The first run is on a [`Profile`] heap, which frees nothing and, before every
//! allocation, finds what the root stack still reaches. An object that was reachable
//! before the last allocation and isn't now has died in between. The result is the
//! [`Lifetimes`] of the run: for each allocation, the objects to free before it.
//!
//! The second run is on an [`Explicit`] heap, which keeps each object in a block from an
//! allocator of the usual kind ([`Subject`]) and frees the blocks the lifetimes name, when
//! they name them. The interpreter and the program are the same in both runs, and the
//! interpreter is deterministic, so the second run makes the same allocations in the same
//! order as the first.

use crate::subjects::Subject;
use green_tea_lisp::{Full, Heap, Kind, Obj, Stats, Value};

/// When the objects of one run of a program died. An object is named by its number: the
/// first one allocated is 0.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Lifetimes {
    /// For each allocation, the objects that became unreachable since the allocation
    /// before it, lowest number first.
    pub deaths: Vec<Vec<u32>>,
    /// The words of each object: its fields and a header.
    pub words: Vec<u8>,
    /// Objects still reachable when the run ended.
    pub left: usize,
    /// Objects that became unreachable after the last allocation. Nothing frees them: the
    /// dead are freed before an allocation, and there is no other one.
    pub died_after_last: usize,
    /// The most words in reachable objects at any allocation.
    pub peak_words: usize,
}

impl Lifetimes {
    pub fn allocations(&self) -> usize {
        self.words.len()
    }

    /// The run as a trace of allocations and frees, with the object's number plus one as
    /// its address: what an allocator would have been asked, had the program freed each
    /// object when it died.
    pub fn events(&self) -> Vec<green_tea_fit::trace::Event> {
        let mut events = Vec::new();
        for (n, deaths) in self.deaths.iter().enumerate() {
            events.extend(deaths.iter().map(|&dead| green_tea_fit::trace::Event::free(dead as usize + 1)));
            events.push(green_tea_fit::trace::Event::allocate(n + 1, self.words[n] as usize * 8, 8));
        }
        events
    }
}

/// A heap that frees nothing and records when each object dies.
#[derive(Default)]
pub struct Profile {
    /// Each object's kind and fields. An object's number is its index.
    objects: Vec<(Kind, [Value; 3])>,
    roots: Vec<Value>,
    /// The objects not yet found dead.
    alive: Vec<u32>,
    /// For each object, the last allocation at which it was reached, plus one.
    reached: Vec<u32>,
    work: Vec<u32>,
    lifetimes: Lifetimes,
    live_words: usize,
}

impl Profile {
    pub fn new() -> Profile {
        Profile::default()
    }

    /// Mark what the root stack reaches, and move everything else from `alive` to the
    /// deaths of the allocation that is about to happen.
    fn find_the_dead(&mut self) {
        let stamp = self.objects.len() as u32 + 1;
        self.mark(stamp);
        let mut dead = Vec::new();
        let (reached, words) = (&self.reached, &self.lifetimes.words);
        let mut freed = 0;
        self.alive.retain(|&n| {
            let kept = reached[n as usize] == stamp;
            if !kept {
                dead.push(n);
                freed += words[n as usize] as usize;
            }
            kept
        });
        self.live_words -= freed;
        self.lifetimes.deaths.push(dead);
    }

    /// Stamp everything the root stack reaches.
    fn mark(&mut self, stamp: u32) {
        self.work.extend(self.roots.iter().filter_map(|v| v.as_obj()).map(|obj| obj.0));
        while let Some(n) = self.work.pop() {
            if self.reached[n as usize] == stamp {
                continue;
            }
            self.reached[n as usize] = stamp;
            let (kind, fields) = &self.objects[n as usize];
            self.work.extend(fields[..kind.fields()].iter().filter_map(|v| v.as_obj()).map(|obj| obj.0));
        }
    }

    /// The lifetimes of the run so far.
    pub fn finish(mut self) -> Lifetimes {
        // One more than any allocation's stamp.
        let stamp = self.objects.len() as u32 + 2;
        self.mark(stamp);
        let left = self.alive.iter().filter(|&&n| self.reached[n as usize] == stamp).count();
        self.lifetimes.left = left;
        self.lifetimes.died_after_last = self.alive.len() - left;
        self.lifetimes
    }
}

impl Heap for Profile {
    fn push(&mut self, v: Value) {
        self.roots.push(v);
    }

    fn pop(&mut self) {
        self.roots.pop().expect("the root stack is empty");
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
        self.find_the_dead();
        let mut fields = [Value::NIL; 3];
        for i in (0..kind.fields()).rev() {
            fields[i] = self.roots.pop().expect("an object's fields weren't on the root stack");
        }
        let n = self.objects.len() as u32;
        self.objects.push((kind, fields));
        self.reached.push(0);
        self.alive.push(n);
        self.lifetimes.words.push(kind.fields() as u8 + 1);
        self.live_words += kind.fields() + 1;
        self.lifetimes.peak_words = self.lifetimes.peak_words.max(self.live_words);
        self.roots.push(Value::obj(Obj(n)));
        Ok(())
    }

    fn kind(&self, obj: Obj) -> Kind {
        self.objects[obj.0 as usize].0
    }

    fn field(&self, obj: Obj, i: usize) -> Value {
        self.objects[obj.0 as usize].1[i]
    }

    fn set_field(&mut self, obj: Obj, i: usize, v: Value) {
        self.objects[obj.0 as usize].1[i] = v;
    }

    fn stats(&self) -> Stats {
        let allocated = self.objects.len() as u64;
        Stats {
            allocated,
            freed: allocated - self.alive.len() as u64,
            live_words: self.live_words,
            peak_words: self.lifetimes.peak_words,
        }
    }
}

/// How far below the first block an [`Explicit`] heap puts the address that references
/// are counted from, in words of 8 bytes: 16 GiB. A reference is 32 bits, so every block
/// must be within 16 GiB of the first on either side.
const REACH: usize = 1 << 31;

/// A heap whose objects are blocks from an allocator, freed when a [`Lifetimes`] says.
///
/// A block is a header word, which holds the object's kind, and the fields. A reference
/// is the block's address, as a count of words from a fixed address.
pub struct Explicit {
    subject: Box<dyn Subject>,
    /// Empty for a heap that never frees.
    deaths: Vec<Vec<u32>>,
    /// The address of each object's block, by the object's number. 0 once it is freed.
    blocks: Vec<usize>,
    /// What references count from.
    anchor: usize,
    roots: Vec<Value>,
    stats: Stats,
    /// In a debug build, the blocks that are allocated, so that every use of an object can
    /// be checked against them.
    #[cfg(debug_assertions)]
    live: std::collections::BTreeSet<usize>,
}

impl Explicit {
    /// A heap over `subject` that frees what `lifetimes` says, when it says. With `None`
    /// it frees nothing.
    pub fn new(subject: Box<dyn Subject>, lifetimes: Option<&Lifetimes>) -> Explicit {
        Explicit {
            subject,
            deaths: lifetimes.map(|lifetimes| lifetimes.deaths.clone()).unwrap_or_default(),
            // Room for a program of any size here, so that the table never grows during a run.
            blocks: Vec::with_capacity(lifetimes.map_or(1 << 16, Lifetimes::allocations)),
            anchor: 0,
            roots: Vec::with_capacity(1024),
            stats: Stats::default(),
            #[cfg(debug_assertions)]
            live: Default::default(),
        }
    }

    pub fn subject(&self) -> &dyn Subject {
        &*self.subject
    }

    fn addr(&self, obj: Obj) -> usize {
        let addr = self.anchor + ((obj.0 as usize) << 3);
        // In a debug build every use of an object checks that the lifetimes haven't had
        // its block freed. A release build is the one that is measured, and doesn't.
        #[cfg(debug_assertions)]
        assert!(self.live.contains(&addr), "an object was used after the oracle freed it");
        addr
    }

    fn kind_of(header: u64) -> Kind {
        match header {
            1 => Kind::Pair,
            2 => Kind::Closure,
            _ => Kind::Binding,
        }
    }
}

impl Heap for Explicit {
    fn push(&mut self, v: Value) {
        self.roots.push(v);
    }

    fn pop(&mut self) {
        self.roots.pop().expect("the root stack is empty");
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
        let n = self.blocks.len();
        // The oracle: free what died since the last allocation.
        if let Some(deaths) = self.deaths.get(n) {
            for &dead in deaths {
                let block = core::mem::take(&mut self.blocks[dead as usize]);
                #[cfg(debug_assertions)]
                assert!(self.live.remove(&block), "the oracle freed an object twice");
                // SAFETY: the block came from `allocate` below, with the size its header
                // gives, and is freed once: its entry is now 0.
                let words = Explicit::kind_of(unsafe { *(block as *const u64) }).fields() + 1;
                unsafe { self.subject.free(block, words * 8, 8) };
                self.stats.freed += 1;
                self.stats.live_words -= words;
            }
        }
        let words = kind.fields() + 1;
        let block = self.subject.allocate(words * 8, 8).ok_or(Full)?;
        if n == 0 {
            self.anchor = block - (REACH << 3);
        }
        let offset = (block.wrapping_sub(self.anchor)) >> 3;
        assert!(offset <= u32::MAX as usize, "a block too far from the first for a reference of 32 bits");
        // SAFETY: the block is `words` words, aligned to 8, and nobody else's.
        unsafe {
            let at = block as *mut u64;
            at.write(match kind {
                Kind::Pair => 1,
                Kind::Closure => 2,
                Kind::Binding => 3,
            });
            for i in (1..words).rev() {
                at.add(i).write(self.roots.pop().expect("an object's fields weren't on the root stack").bits());
            }
        }
        self.blocks.push(block);
        #[cfg(debug_assertions)]
        self.live.insert(block);
        self.stats.allocated += 1;
        self.stats.live_words += words;
        self.stats.peak_words = self.stats.peak_words.max(self.stats.live_words);
        self.roots.push(Value::obj(Obj(offset as u32)));
        Ok(())
    }

    fn kind(&self, obj: Obj) -> Kind {
        // SAFETY: a reference the interpreter holds is to a block that hasn't been freed,
        // if the lifetimes are those of this program.
        Explicit::kind_of(unsafe { *(self.addr(obj) as *const u64) })
    }

    fn field(&self, obj: Obj, i: usize) -> Value {
        // SAFETY: as for `kind`; the interpreter asks for a field the kind has.
        Value::from_bits(unsafe { *(self.addr(obj) as *const u64).add(i + 1) })
    }

    fn set_field(&mut self, obj: Obj, i: usize, v: Value) {
        // SAFETY: as for `field`.
        unsafe { (self.addr(obj) as *mut u64).add(i + 1).write(v.bits()) }
    }

    fn stats(&self) -> Stats {
        self.stats
    }
}
