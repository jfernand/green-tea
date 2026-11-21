//! A small Lisp, and the heap its objects live in.
//!
//! The language: integers, symbols, pairs, `nil` and `#t`; `quote`, `if`, `define` and
//! `lambda`, with lexical closures and tail calls; thirteen primitives ([`eval::PRIMITIVES`]).
//! [`Lisp`] is the reader, the evaluator and the printer.
//!
//! The interpreter never sees memory. It reaches its objects through the [`Heap`] trait,
//! and this crate's first implementation of it, [`rc::RcHeap`], counts references. A heap
//! that traces, or one that copies, can take its place without a change to the evaluator,
//! because of what the interface does and doesn't let the evaluator do. Four do:
//! [`mark::MarkHeap`] marks what the root stack reaches and sweeps or compacts the rest,
//! [`semispace::Semispace`] copies what it reaches, [`generational::GenHeap`] keeps young
//! objects apart from old ones, and [`incremental::IncHeap`] marks and sweeps a little at
//! a time. [`gc`] has what they share.
//!
//! # The interface
//!
//! A [`Value`] is one word: an integer, a symbol, a primitive, `nil`, `#t`, or a
//! *reference* to an object ([`Obj`], an index into the heap, never an address). An object
//! has a [`Kind`] and two or three fields, each a `Value`.
//!
//! The heap keeps the *root stack*: the values the interpreter is holding. Everything the
//! interpreter does to objects is one of these ten operations.
//!
//! | operation | what it does |
//! |---|---|
//! | `push(v)` | hold `v`: put it on top of the root stack |
//! | `pop()` | stop holding the top of the root stack |
//! | `roots()` | how many values are on the root stack |
//! | `root(slot)` | the value in a slot, counted from the bottom |
//! | `set_root(slot, v)` | replace the value in a slot |
//! | `alloc(kind)` | make an object: its fields are popped from the root stack (the last field from the top), and the new object is pushed in their place |
//! | `kind(obj)` | what an object is |
//! | `field(obj, i)` | read a field |
//! | `set_field(obj, i, v)` | write a field: **the only way a reference is stored in an object that exists** |
//! | `stats()` | allocations, frees, and words in use |
//!
//! # The rule
//!
//! A reference the interpreter has in a Rust variable is *borrowed*: `root` and `field`
//! hand out copies that nothing accounts for. A borrowed reference is good until the next
//! call of `alloc`, `pop`, `set_root` or `set_field`. To keep a value across one of those,
//! the interpreter pushes it, and reads it back from its slot afterwards.
//!
//! Each kind of heap needs the rule for a reason of its own. A reference-counting heap
//! frees an object when the last counted reference to it goes, and only root slots and
//! fields are counted, so a `pop` can free what a borrowed reference names. A tracing heap
//! may collect inside any `alloc`, and it keeps only what it can reach from the root
//! stack. A copying heap moves objects inside `alloc`, and corrects the references it can
//! find: the ones in root slots and in fields. So `alloc` takes its fields from the root
//! stack and not from arguments, which would be borrowed, and stale by the time a
//! collection that `alloc` started had finished.
//!
//! What the interpreter may not do: keep an [`Obj`] in a structure of its own, compute
//! one, or assume two calls of `root` on the same slot return the same reference (a
//! copying heap may have moved the object in between). `==` on two `Value`s read without
//! an `alloc` between them is identity.

#![no_std]

extern crate alloc;

pub mod eval;
pub mod gc;
pub mod generational;
pub mod incremental;
pub mod mark;
pub mod print;
pub mod programs;
pub mod rc;
pub mod read;
pub mod semispace;

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod gc_tests;
#[cfg(test)]
mod tests;

pub use eval::{Error, Lisp};

/// A reference to an object: its index in the heap. Only a heap makes one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Obj(pub u32);

/// One word: an immediate, or a reference to an object. The low three bits are a tag.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Value(u64);

const TAG_BITS: u32 = 3;
const TAG_MASK: u64 = 0b111;
const TAG_INT: u64 = 0;
const TAG_OBJ: u64 = 1;
const TAG_SYM: u64 = 2;
const TAG_PRIM: u64 = 3;
const TAG_CONST: u64 = 4;

/// A [`Value`], taken apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Val {
    Nil,
    True,
    /// An integer of 61 bits.
    Int(i64),
    /// A symbol, by its number in the interpreter's table.
    Sym(u32),
    /// A primitive, by its index in [`eval::PRIMITIVES`].
    Prim(u32),
    Obj(Obj),
}

impl Value {
    pub const NIL: Value = Value(TAG_CONST);
    pub const TRUE: Value = Value(1 << TAG_BITS | TAG_CONST);
    /// The largest and the smallest integer a value holds: 61 bits.
    pub const MAX_INT: i64 = i64::MAX >> TAG_BITS;
    pub const MIN_INT: i64 = i64::MIN >> TAG_BITS;

    /// Arithmetic wraps at 61 bits.
    pub fn int(n: i64) -> Value {
        Value((n << TAG_BITS) as u64 | TAG_INT)
    }

    pub fn sym(n: u32) -> Value {
        Value(u64::from(n) << TAG_BITS | TAG_SYM)
    }

    pub fn prim(n: u32) -> Value {
        Value(u64::from(n) << TAG_BITS | TAG_PRIM)
    }

    pub fn obj(obj: Obj) -> Value {
        Value(u64::from(obj.0) << TAG_BITS | TAG_OBJ)
    }

    pub fn decode(self) -> Val {
        let rest = self.0 >> TAG_BITS;
        match self.0 & TAG_MASK {
            TAG_INT => Val::Int(self.0 as i64 >> TAG_BITS),
            TAG_OBJ => Val::Obj(Obj(rest as u32)),
            TAG_SYM => Val::Sym(rest as u32),
            TAG_PRIM => Val::Prim(rest as u32),
            _ if self == Value::NIL => Val::Nil,
            _ => Val::True,
        }
    }

    /// The object this value refers to, if it is a reference. This is the question a heap
    /// asks of every word it stores.
    pub fn as_obj(self) -> Option<Obj> {
        if self.0 & TAG_MASK == TAG_OBJ { Some(Obj((self.0 >> TAG_BITS) as u32)) } else { None }
    }

    /// The word itself, for a heap to store.
    pub fn bits(self) -> u64 {
        self.0
    }

    /// A value from a word that `bits` returned.
    pub fn from_bits(bits: u64) -> Value {
        Value(bits)
    }
}

impl core::fmt::Debug for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.decode().fmt(f)
    }
}

/// What an object is, which says how many fields it has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `car`, `cdr`.
    Pair,
    /// A function: its parameters (a list of symbols), its body (a list of expressions),
    /// and the environment it was made in.
    Closure,
    /// One variable of an environment: its name (a symbol), its value, and the rest of
    /// the environment (a binding, or `nil`).
    Binding,
}

impl Kind {
    pub const fn fields(self) -> usize {
        match self {
            Kind::Pair => 2,
            Kind::Closure | Kind::Binding => 3,
        }
    }
}

/// The heap has no room for the object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Full;

/// What every heap counts. A word is 8 bytes; an object is its fields and whatever the
/// heap keeps beside them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Objects allocated, ever.
    pub allocated: u64,
    /// Objects freed, ever.
    pub freed: u64,
    /// Words in objects that are allocated and not freed.
    pub live_words: usize,
    /// The most `live_words` has been.
    pub peak_words: usize,
}

impl Stats {
    /// Objects allocated and not freed.
    pub fn live(&self) -> u64 {
        self.allocated - self.freed
    }
}

/// Where the interpreter's objects live. The module documentation has the rule that goes
/// with these operations.
pub trait Heap {
    /// Hold `v`: put it on top of the root stack.
    fn push(&mut self, v: Value);

    /// Stop holding the top of the root stack.
    ///
    /// # Panics
    /// If the root stack is empty.
    fn pop(&mut self);

    /// How many values are on the root stack.
    fn roots(&self) -> usize;

    /// The value in a slot of the root stack; slot 0 is the bottom. Borrowed.
    fn root(&self, slot: usize) -> Value;

    /// Replace the value in a slot of the root stack.
    fn set_root(&mut self, slot: usize, v: Value);

    /// Make an object of `kind`. Its fields are the top `kind.fields()` values of the root
    /// stack, the last field on top; they are popped, and the new object is pushed.
    /// On `Err` the root stack is as it was.
    fn alloc(&mut self, kind: Kind) -> Result<(), Full>;

    /// What an object is.
    fn kind(&self, obj: Obj) -> Kind;

    /// Read a field. Borrowed.
    fn field(&self, obj: Obj, i: usize) -> Value;

    /// Write a field. Every reference stored in an object after `alloc` made it goes
    /// through here, so this is where a heap that must know about such writes finds out.
    fn set_field(&mut self, obj: Obj, i: usize, v: Value);

    fn stats(&self) -> Stats;

    /// The top of the root stack. Borrowed.
    fn top(&self) -> Value {
        self.root(self.roots() - 1)
    }
}
