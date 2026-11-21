#import "../lib.typ": *

= green-tea-lisp <crate-lisp>

`green-tea-lisp` is a small Lisp and six heaps to run it on. The language has integers, symbols, pairs, `nil` and `#t`, the forms `quote`, `if`, `define` and `lambda`, lexical closures, tail calls and thirteen primitives. The interpreter never sees memory: it reaches its objects through the `Heap` trait, and a reference-counting heap, a mark-sweep heap, a mark-compact heap, a copying heap, a generational heap and an incremental heap all run it unchanged. (`MarkHeap` is two of the six, by a parameter.)

#spec(
  ("Source", [#repo("green-tea-lisp/src/"), 11 files, 3,600 lines without their tests; 9 programs in #repo("green-tea-lisp/programs/"); 4 examples, 804 lines]),
  ("Depends on", [`alloc`; `no_std`]),
  ("Used by", [`green-tea-coop` (the `lisp`, `gc`, `gen` and `incremental` kernels), `green-tea-measure`]),
  ("Tests", [161 host tests, the examples `heaps`, `generations`, `incremental` and `store_cost`, four kernels]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [crate root], [`Value`, `Val`, `Obj`, `Kind`, `Stats`, `Full`, the `Heap` trait],
  [`read`], [the reader: `Text`, `Lisp::read`],
  [`eval`], [the evaluator: `Lisp`, `Error`, `PRIMITIVES`, `QUOTE`, `MAX_DEPTH`],
  [`print`], [the printer: `Lisp::print`],
  [`programs`], [`Program`, `PROGRAMS`],
  [`rc`], [`RcHeap`, `Freeing`, `Cycles`, `Counts`, `LAZY_STEP`],
  [`gc`], [`Collect`, `Counts`, `reachable`: what the tracing heaps share],
  [`mark`], [`MarkHeap`, `Reclaim`],
  [`semispace`], [`Semispace`],
  [`generational`], [`GenHeap`, `Barrier`, `Generations`, `MUTATING`],
  [`incremental`], [`IncHeap`, `Barrier`, `Birth`, `Pace`, `Phase`, `Colour`, `Steps`, `PAUSE_BUCKETS`, `MUTATORS`],
)

== The heap interface

=== Values

A `Value` is one 64-bit word, and its low three bits are a tag:

#srcfile("green-tea-lisp/src/lib.rs", lines: ((87, 87), (89, 95), (112, 113), (116, 118), (128, 130),))[```rust
pub struct Value(u64);
// …
const TAG_BITS: u32 = 3;
const TAG_MASK: u64 = 0b111;
const TAG_INT: u64 = 0;
const TAG_OBJ: u64 = 1;
const TAG_SYM: u64 = 2;
const TAG_PRIM: u64 = 3;
const TAG_CONST: u64 = 4;
// …
    pub const NIL: Value = Value(TAG_CONST);
    pub const TRUE: Value = Value(1 << TAG_BITS | TAG_CONST);
// …
    pub fn int(n: i64) -> Value {
        Value((n << TAG_BITS) as u64 | TAG_INT)
    }
// …
    pub fn obj(obj: Obj) -> Value {
        Value(u64::from(obj.0) << TAG_BITS | TAG_OBJ)
    }
```]

#bytefield(caption: [A `Value`: an integer has 61 bits, and any other value keeps its payload above the tag],
  ("payload", "61 bits", 6), ("tag", "3 bits", 1))

#dtable(columns: (auto, auto, 1fr), ("Tag", "Value", "Payload"),
  [0], [integer], [a signed 61-bit number, so `Value::int` and arithmetic wrap at 61 bits],
  [1], [reference], [an `Obj`: the object's index in the heap, never an address],
  [2], [symbol], [its number in the interpreter's table],
  [3], [primitive], [its index in `PRIMITIVES`],
  [4], [constant], [0 is `Value::NIL`, 1 is `Value::TRUE`],
)

`Value::int`, `sym`, `prim` and `obj` build one. `decode` takes it apart into a `Val`, an enum with a variant for each. `as_obj` is the question a heap asks of every word it stores: is this a reference? `bits` and `from_bits` move the word in and out of a heap's array.

`Kind` says what an object is and so how many `fields` it has: a `Pair` has two, a `Closure` (parameters, body, environment) three, and a `Binding` (name, value, rest of the environment) three. `Full` is the error for a heap with no room. `Stats` is what every heap counts: `allocated`, `freed`, `live_words`, `peak_words`, and the derived `live`, which is allocated minus freed.

=== The `Heap` trait

#srcfile("green-tea-lisp/src/lib.rs", lines: ((216, 216), (218, 218), (224, 224), (227, 227), (230, 230), (233, 233), (238, 238), (241, 241), (244, 244), (248, 248), (250, 250), (253, 256),))[```rust
pub trait Heap {
// …
    fn push(&mut self, v: Value);
// …
    fn pop(&mut self);
// …
    fn roots(&self) -> usize;
// …
    fn root(&self, slot: usize) -> Value;
// …
    fn set_root(&mut self, slot: usize, v: Value);
// …
    fn alloc(&mut self, kind: Kind) -> Result<(), Full>;
// …
    fn kind(&self, obj: Obj) -> Kind;
// …
    fn field(&self, obj: Obj, i: usize) -> Value;
// …
    fn set_field(&mut self, obj: Obj, i: usize, v: Value);
// …
    fn stats(&self) -> Stats;
// …
    fn top(&self) -> Value {
        self.root(self.roots() - 1)
    }
}
```]

The heap keeps the *root stack*, the values the interpreter is holding. `alloc` takes its fields from the top of that stack and pushes the new object in their place, so nothing the collector needs to see is ever only in a Rust variable. `set_field` is the only way a reference is stored into an object that exists, and that is where the write barriers of the generational and incremental heaps go.

*A reference the interpreter has in a Rust variable is borrowed, and is good only until the next call of `alloc`, `pop`, `set_root` or `set_field`.* To keep a value across one, the interpreter pushes it, and reads it back from its slot. Each kind of heap needs the rule for its own reason: a counting heap can free what a `pop` dropped, a tracing heap keeps only what it reaches from the root stack, and a copying heap moves objects inside `alloc` and corrects only the references it can find.

== The reader

`read` turns text into objects, directly on the root stack. `Text` is a position in the source, `Text::new` makes one, and `more` says whether anything but blanks and `;` comments is left. A list is read by pushing each element, then `nil`, and allocating one pair per element, last first, because `alloc` pops a `car` and a `cdr` and pushes the pair that is the next `cdr`:

#srcfile("green-tea-lisp/src/read.rs", lines: ((51, 95),), notes: ((52, [`MAX_NESTING` is 32, because the reader recurses.]), (69, [The list exists only on the root stack until here, so a collection during any nested `alloc` sees all of it.]), (75, [`'x` is read as `(quote x)`.]),), size: 6.3pt)[```rust
fn read_nested(&mut self, text: &mut Text, nesting: usize) -> Result<(), Error> {
    if nesting > MAX_NESTING {
        return Err(Error::Syntax("lists nested too deeply"));
    }
    match text.peek().ok_or(Error::Syntax("the text ended inside an expression"))? {
        b'(' => {
            text.at += 1;
            let mut elements = 0;
            loop {
                match text.peek() {
                    None => return Err(Error::Syntax("a list without its )")),
                    Some(b')') => break,
                    Some(_) => self.read_nested(text, nesting + 1)?,
                }
                elements += 1;
            }
            text.at += 1;
            self.heap.push(Value::NIL);
            for _ in 0..elements {
                self.heap.alloc(Kind::Pair)?;
            }
            Ok(())
        }
        b')' => Err(Error::Syntax("a ) with no list open")),
        b'\'' => {
            text.at += 1;
            self.heap.push(Value::sym(QUOTE));
            self.read_nested(text, nesting + 1)?;
            self.heap.push(Value::NIL);
            self.heap.alloc(Kind::Pair)?;
            self.heap.alloc(Kind::Pair)?; // (quote x)
            Ok(())
        }
        _ => {
            let start = text.at;
            while text.bytes.get(text.at).is_some_and(|&b| !b.is_ascii_whitespace() && !b"()';".contains(&b)) {
                text.at += 1;
            }
            let atom = core::str::from_utf8(&text.bytes[start..text.at]).map_err(|_| Error::Syntax("not UTF-8"))?;
            let atom = self.atom(atom);
            self.heap.push(atom);
            Ok(())
        }
    }
}
```]

`atom` reads `nil`, `#t`, then an `i64`, and anything else is a symbol, interned by the interpreter. There is no syntax for a dotted pair.

== The evaluator

=== `Lisp`

`Lisp<H>` holds the heap, a table of symbol names and the depth of nesting. `Lisp::new` pushes `nil` as the global environment, in root slot 0, and interns the special forms and `PRIMITIVES` first, so their numbers are constants (`QUOTE` is 0):

#srcfile("green-tea-lisp/src/eval.rs", lines: ((26, 27), (35, 35), (38, 52),), size: 6.3pt)[```rust
pub const PRIMITIVES: [&str; 13] =
    ["car", "cdr", "cons", "set-car!", "set-cdr!", "+", "-", "*", "remainder", "<", "=", "eq?", "null?"];
// …
pub const MAX_DEPTH: usize = 32;
// …
pub enum Error {
    Syntax(&'static str),
    /// A symbol with no value, by its number: see [`Lisp::name`].
    Unbound(u32),
    NotAFunction,
    NotAPair,
    NotANumber,
    /// The wrong number of arguments.
    Arguments,
    DivisionByZero,
    /// More than [`MAX_DEPTH`] calls nested.
    TooDeep,
    /// The heap has no room.
    HeapFull,
}
```]

`intern` returns a symbol's number and `name` its text. `run` reads and evaluates every expression in a string, returns the last value printed, and whatever happens leaves the root stack holding the global environment alone. `global` reads a definition, `forget_globals` drops them all (what the heap still holds afterwards is what it can't free), and `deepest` reports the most nested calls of `eval`.

=== `eval` and tail calls

`eval` takes an expression and an environment in the top two root slots and leaves the value in their place. It nests at most `MAX_DEPTH` (32) calls, because the kernels run the interpreter on their boot stack. Calls in tail position don't nest:

#srcfile("green-tea-lisp/src/eval.rs", lines: ((172, 206),), notes: ((177, [The test nests. The branch doesn't: `choose` puts it in slot `base` and the loop goes round.]), (190, [`operand` pushes one element of the call and the environment, and `eval` replaces them with its value.]), (194, [`apply` replaces the expression and environment in slots `base` and `base + 1` with the callee's: a tail call.]), (199, [All but the last expression of a body are evaluated and dropped. The last goes round the loop.]),))[```rust
fn eval_at(&mut self, base: usize) -> Result<(), Error> {
    loop {
        match self.classify(base)? {
            Form::Value => return Ok(()),
            Form::If => {
                self.eval()?; // the test, which `classify` pushed
                self.choose(base); // the branch is in tail position
            }
            Form::Define => {
                if self.defined_value(base)? {
                    self.eval()?;
                }
                return self.define(base);
            }
            Form::Call => {
                // Evaluate the function and each argument, leaving them on the root
                // stack: the function in slot `base + 2`, the arguments after it.
                let mut count = 0;
                while self.operand(base, count)? {
                    self.eval()?;
                    count += 1;
                }
                if self.apply(base, count - 1)? == Applied::Primitive {
                    return Ok(());
                }
                // Slot `base` now holds the function's body, a list of expressions.
                // All but the last are evaluated for what they do.
                while self.statement(base)? {
                    self.eval()?;
                    self.heap.pop();
                }
            }
        }
    }
}
```]

`eval_at` and `eval` are the only two functions on the Rust stack once per nested call. Everything that needs variables is in a helper that returns before `eval` is called again. `classify` looks at the expression: a constant, a variable, a `quote` or a `lambda` has its value pushed, and an `if`, `define` or call is reported for `eval_at` to run.

=== `apply`

#srcfile("green-tea-lisp/src/eval.rs", lines: ((276, 312),), notes: ((288, [The callee's environment is built in a root slot.]), (292, [The closure is read again from its slot on every round, because the `alloc` below may have moved it.]), (306, [Body and environment replace the caller's: no root slot and no Rust frame is left behind.]),))[```rust
fn apply(&mut self, base: usize, arguments: usize) -> Result<Applied, Error> {
    let function = base + 2;
    let closure = match self.heap.root(function).decode() {
        Val::Prim(primitive) => {
            self.primitive(primitive, function + 1, arguments)?;
            return Ok(Applied::Primitive);
        }
        Val::Obj(obj) if self.heap.kind(obj) == Kind::Closure => obj,
        _ => return Err(Error::NotAFunction),
    };

    // The callee's environment: the closure's, with a binding for each parameter.
    let callee = self.heap.roots();
    self.heap.push(self.heap.field(closure, 2));
    for i in 0..arguments {
        // The closure is read again each time round: `alloc` came in between.
        let closure = self.heap.root(function).as_obj().expect("the closure");
        let name = self.nth(self.heap.field(closure, 0), i).map_err(|_| Error::Arguments)?;
        self.heap.push(name);
        self.heap.push(self.heap.root(function + 1 + i));
        self.heap.push(self.heap.root(callee));
        self.heap.alloc(Kind::Binding)?;
        self.heap.set_root(callee, self.heap.top());
        self.heap.pop();
    }
    let closure = self.heap.root(function).as_obj().expect("the closure");
    if self.tail(self.heap.field(closure, 0), arguments)? != Value::NIL {
        return Err(Error::Arguments);
    }

    self.heap.set_root(base, self.heap.field(closure, 1));
    self.heap.set_root(base + 1, self.heap.root(callee));
    while self.heap.roots() > base + 2 {
        self.heap.pop();
    }
    Ok(Applied::Closure)
}
```]

`define` always binds in the global environment, in the chain in root slot 0, and overwrites an existing binding with `set_field`. A variable is looked up in the local chain, then the global one, and finally as the name of a primitive, which is how `(define car ...)` can shadow one. The thirteen primitives are `car`, `cdr`, `cons`, `set-car!`, `set-cdr!`, `+`, `-`, `*`, `remainder`, `<`, `=`, `eq?` and `null?`. `eq?` compares words, so it is identity for objects.

`Error` has `Syntax`, `Unbound`, `NotAFunction`, `NotAPair`, `NotANumber`, `Arguments`, `DivisionByZero`, `TooDeep` and `HeapFull`.

== The printer

`print` writes a value as the reader would read it, where it can be read. A closure is `#<function>`, a binding `#<environment>`, a primitive `#<primitive car>`. Printing has a budget, so that a cycle ends: 64 atoms and a nesting of 16, after which it prints `...`:

#srcfile("green-tea-lisp/src/print.rs", lines: ((35, 57),))[```rust
out.write_char('(')?;
self.print_nested(self.heap.field(pair, 0), out, budget, nesting + 1)?;
let mut rest = self.heap.field(pair, 1);
loop {
    match rest.decode() {
        Val::Nil => break,
        Val::Obj(obj) if self.heap.kind(obj) == Kind::Pair => {
            out.write_char(' ')?;
            if *budget == 0 {
                out.write_str("...")?;
                break;
            }
            self.print_nested(self.heap.field(obj, 0), out, budget, nesting + 1)?;
            rest = self.heap.field(obj, 1);
        }
        _ => {
            out.write_str(" . ")?;
            self.print_nested(rest, out, budget, nesting + 1)?;
            break;
        }
    }
}
out.write_char(')')
```]

== The programs

The heaps are measured with nine programs, each a file that `include_str!` puts in the binary, with the value its last expression must have. `Program` has `name`, `source`, `value` and `cyclic`. `PROGRAMS` holds the first five. `MUTATING` (in `generational`) and `MUTATORS` (in `incremental`) hold two each, which store into existing objects.

#dtable(columns: (auto, auto, auto, 1fr), ("Program", "Value", "In", "What it stresses"),
  [`list`], [50500], [`PROGRAMS`], [short-lived pairs: every list is garbage by the next round],
  [`fib`], [610], [`PROGRAMS`], [environments: 1,973 calls and no data structure],
  [`alist`], [250], [`PROGRAMS`], [sharing: an update copies a prefix of a table],
  [`ring`], [7200], [`PROGRAMS`], [cycles: 100 rings of 8 pairs (`cyclic`)],
  [`closure`], [5500], [`PROGRAMS`], [cycles through a closure's environment (`cyclic`)],
  [`counters`], [250], [`MUTATING`], [stores of integers into long-lived pairs],
  [`slots`], [136], [`MUTATING`], [stores of new objects into old ones],
  [`nreverse`], [20100], [`MUTATORS`], [a list reversed in place, every `cdr` overwritten],
  [`queue`], [125250], [`MUTATORS`], [a long-lived pair written all the time],
)

#srcfile("green-tea-lisp/programs/list.lisp", lines: ((1, 7),))[```lisp
; Builds the list (1 2 ... 100), reverses it and adds it up, ten times over.
; Every pair is garbage by the time the next round starts.
(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc))))
(define (reverse l acc) (if (null? l) acc (reverse (cdr l) (cons (car l) acc))))
(define (sum l acc) (if (null? l) acc (sum (cdr l) (+ acc (car l)))))
(define (rounds k total)
  (if (= k 0) total
```]

#srcfile("green-tea-lisp/programs/fib.lisp", lines: ((1, 4),))[```lisp
; The naive Fibonacci: 1,973 calls, and no data structure at all.
; What it allocates is environments, one binding for each call.
(define (fib n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))
(fib 15)
```]

#srcfile("green-tea-lisp/programs/alist.lisp", lines: ((1, 16),))[```lisp
; An association list used as a table of 16 counters. An update makes a new list, which
; shares with the old one every pair after the one that changed.
(define (lookup key table)
  (if (null? table) 0
      (if (= key (car (car table))) (cdr (car table))
          (lookup key (cdr table)))))
(define (update key value table)
  (if (null? table) (cons (cons key value) nil)
      (if (= key (car (car table))) (cons (cons key value) (cdr table))
          (cons (car table) (update key value (cdr table))))))
(define (count i table)
  (if (= i 0) table
      (count (- i 1)
             ((lambda (key) (update key (+ 1 (lookup key table)) table))
              (remainder (* i 7) 16)))))
(define (total key table acc)
```]

#srcfile("green-tea-lisp/programs/ring.lisp", lines: ((1, 7),))[```lisp
; Makes 100 rings: lists of 8 whose last pair points back at the first. Each ring is
; walked twice round and then dropped.
(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc))))
(define (last l) (if (null? (cdr l)) l (last (cdr l))))
(define (ring n) ((lambda (l) (set-cdr! (last l) l) l) (iota n nil)))
(define (walk l k acc) (if (= k 0) acc (walk (cdr l) (- k 1) (+ acc (car l)))))
(define (rings k total)
```]

#srcfile("green-tea-lisp/programs/closure.lisp", lines: ((1, 9),))[```lisp
; A function that calls itself through the pair it is stored in. The function's
; environment holds the pair, and the pair holds the function: a cycle of three objects,
; made 100 times.
(define (make-adder)
  ((lambda (cell)
     (set-car! cell (lambda (n acc) (if (= n 0) acc ((car cell) (- n 1) (+ acc n)))))
     (car cell))
   (cons nil nil)))
(define (repeat k total)
```]

#srcfile("green-tea-lisp/programs/counters.lisp", lines: ((1, 10),))[```lisp
; The table of alist.lisp, kept in place: sixteen counters that live for the whole run,
; each a pair (key . count) whose cdr is overwritten. What is stored is an integer.
(define (counters n acc) (if (= n 0) acc (counters (- n 1) (cons (cons (- n 1) 0) acc))))
(define table (counters 16 nil))
(define (find key t) (if (= key (car (car t))) (car t) (find key (cdr t))))
(define (add1 cell) (set-cdr! cell (+ 1 (cdr cell))))
(define (count i)
  (add1 (find (remainder (* i 7) 16) table))
  (if (= i 1) nil (count (- i 1))))
(count 250)
```]

#srcfile("green-tea-lisp/programs/slots.lisp", lines: ((1, 10),))[```lisp
; Sixteen slots that live for the whole run. Each step makes a list of three numbers and
; stores it in a slot, in place of the list that was there: a reference to a new object,
; stored in an old one.
(define (slots n acc) (if (= n 0) acc (slots (- n 1) (cons (cons (- n 1) nil) acc))))
(define table (slots 16 nil))
(define (find key t) (if (= key (car (car t))) (car t) (find key (cdr t))))
(define (fill i)
  (set-cdr! (find (remainder (* i 7) 16) table) (cons i (cons i (cons i nil))))
  (if (= i 1) nil (fill (- i 1))))
(fill 250)
```]

#srcfile("green-tea-lisp/programs/nreverse.lisp", lines: ((1, 7),))[```lisp
; Reverses a list of 200 in place, 20 times, and adds it up. No pair of the list is ever
; garbage: each round turns every cdr round to point at the pair that was before it.
(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc))))
(define (nreverse l acc)
  (if (null? l) acc
      ((lambda (rest) (set-cdr! l acc) (nreverse rest l)) (cdr l))))
(define (sum l acc) (if (null? l) acc (sum (cdr l) (+ acc (car l)))))
```]

#srcfile("green-tea-lisp/programs/queue.lisp", lines: ((1, 14),))[```lisp
; A queue: a pair whose car is the first pair of a list and whose cdr is the last. Five
; hundred numbers go through it, with never more than eight waiting, and are added up as they
; come out. The pair that is the queue lives for the whole run, and is written all the time.
(define (enqueue q x)
  ((lambda (cell)
     (if (null? (car q)) (set-car! q cell) (set-cdr! (cdr q) cell))
     (set-cdr! q cell))
   (cons x nil)))
(define (dequeue q)
  ((lambda (first) (set-car! q (cdr first)) (car first)) (car q)))
(define (drain q acc) (if (null? (car q)) acc (drain q (+ acc (dequeue q)))))
(define (feed q i n acc)
  (if (< n i) (drain q acc)
      ((lambda (ignored) (feed q (+ i 1) n (if (< 8 i) (+ acc (dequeue q)) acc)))
```]

== Reference counting: `RcHeap`

=== Layout

The heap is an array of words. An object is a header followed by its fields, and a reference is the index of the header. Word 0 is never used, so 0 can end a list:

#bytefield(caption: [An `RcHeap` header],
  ("count", "32 bits", 4), ("unused", "25", 3), ("FREE", "1", 1), ("DEAD", "1", 1), ("BUFFERED", "1", 1.4), ("colour", "2", 1), ("kind", "2", 1))

`push`, `set_root` and `set_field` raise the count of what they store. `pop`, `set_root` and `set_field` lower the count of what they drop, and both of the writers increment *first*, because the old value may be all that holds the new one. `alloc` changes no count: the fields move from the root stack into the object, and the object's one reference is the slot it is pushed to.

`RcHeap::new` takes the words, a `Freeing` and a `Cycles`. Freed objects go on one of two free lists, by size, linked through the first field, and `alloc` takes from them before fresh words.

=== Freeing

#srcfile("green-tea-lisp/src/rc.rs", lines: ((252, 274),), notes: ((262, [`DEAD` keeps a freed object from being used, and is what `live` checks on every access.]), (267, [A dead object needs no count, so the queue's link goes in that field.]), (272, [A count that went down without reaching zero may have cut off a cycle.]),))[```rust
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
```]

`Freeing` is the choice of how a chain of deaths is handled:

- `Recursive` drops each field by a call that may free in turn. The stack grows with the longest chain that dies together.
- `Queued` threads dead objects through their count fields and empties the list before the operation returns. Constant stack, the same work.
- `Lazy` puts them on the list and returns. Each `alloc` takes `LAZY_STEP` (2) off it, so work per operation is bounded and memory comes back late. `finish_freeing` empties the queue, and `alloc` does so when it is out of room.

The `lisp` kernel drops a list with one `pop` each way:

#output(title: "cargo xtask kernel --kernel lisp --target riscv64")[
```
a list dropped by one pop:
  recursive: 16 pairs, freed by a recursion 16 deep
    stack: 4320 bytes, 288 for each pair; a task's 16384 would hold 56 pairs
  queued: 10000 pairs; the pop freed 10000, and the recursion was 0 deep
  lazy: 10000 pairs; the pop freed 0, and the recursion was 0 deep
    10000 allocations later: 10000 freed, at most 2 at one allocation; 30000 words touched
```
]

=== Cycles

Counts can't free a cycle. `Cycles::Leak` leaves it allocated. `Cycles::Collect { every }` adds Bacon and Rajan's trial deletion: every object whose count was lowered to a value above zero is a *candidate*, and `collect_cycles` runs when `every` are remembered, or when the heap is full.

#srcfile("green-tea-lisp/src/rc.rs", lines: ((375, 402),), notes: ((383, [An object that died while on the candidate list stays allocated until here, because the list still names it.]), (385, [`mark_gray` takes one from the count of each object for each reference from a gray object.]), (392, [`scan`: a gray object with a count above zero is held from outside. It and all it reaches get their counts back (`scan_black`). The rest turn white.]), (397, [`collect_white` frees what is white.]),))[```rust
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
```]

The colours are Bacon and Rajan's: black (in use), gray (being searched), white (garbage, unless a later step says otherwise) and purple (a candidate). `increment` turns an object black again, so a candidate that gained a reference is dropped from the search.

Besides the heap operations, `RcHeap` has `count` (an object's count), `capacity`, `touched` (words ever used), `audit` (recounts every reference from the roots and fields and compares them with the headers) and `reachable`, a tracing collector's question asked to count what leaked. `rc::Counts` separates `root_increments`, `root_decrements`, `field_increments` and `field_decrements`, sums them in `increments` and `decrements`, and records `largest_burst`, `deepest`, `deepest_bytes`, `candidates`, `searches`, `trial_decrements` and `cycle_freed`.

Most of the counting is for the root stack, where a tracing heap does none:

#output(title: "the same run")[
```
five programs, each in a heap of its own, with its definitions forgotten at the end:
  program    value  allocated     freed    left  peak words  words touched  increments  decrements
  list       50500       8190      8190       0         614            614      105712      113902
  fib          610       2002      2002       0         134            143       39455       41457
  alist        250      14309     14309       0         766            811      229932      244241
  ring        7200       9130      8330     800        2772           2772      121037      129367
  closure     5500       2877      2556     321        1326           1326       39921       42279
where the counting is done: for a root slot, or for a field of an object:
  program   root inc   root dec  field inc  field dec  for roots  per allocation
  list         105712     104771          0       9131        95%            26
  fib           39455      41429          0         28        99%            40
  alist        229932     227648          0      16593        96%            33
  ring         120937     121035        100       8332        96%            27
  closure       39821      39723        100       2556        96%            28
```
]

`ring` leaves its 800 pairs, 100 rings of 8, and `closure` 321 objects. With trial deletion in a small heap, both finish, and nothing is left:

#output(title: "the same run")[
```
the programs that make cycles, in a heap of 1024 words:
  ring     counts alone:    Err(HeapFull) after 2567 allocations, 2351 freed
  ring     trial deletion:  Ok("7200") after 9130 allocations, 9130 freed; 67 searches freed 800
  closure  counts alone:    Err(HeapFull) after 2099 allocations, 1859 freed
  closure  trial deletion:  Ok("5500") after 2877 allocations, 2877 freed; 14 searches freed 321
```
]

== What the tracing heaps share: `gc`

A tracing heap frees nothing when a reference is dropped. When it has no room it *collects*: it starts from the root stack, follows every reference it finds, and keeps what it reached. All four implement `Collect`:

#srcfile("green-tea-lisp/src/gc.rs", lines: ((68, 68), (70, 70), (72, 72), (75, 75), (80, 80), (83, 84),))[```rust
pub trait Collect: Heap {
// …
    fn collect(&mut self);
// …
    fn counts(&self) -> Counts;
// …
    fn capacity(&self) -> usize;
// …
    fn set_stress(&mut self, stress: bool);
// …
    fn set_clock(&mut self, clock: fn() -> u64);
}
```]

`set_stress` collects at every allocation (`IncHeap` starts a cycle at each one that finds none in progress), so a reference the interpreter kept where the heap can't see it goes wrong at the first allocation and not at a rare one. `set_clock` gives the heap a clock in ticks of any kind. `gc::Counts` holds `collections`, `traced`, `traced_words`, `roots`, `walked`, `moved_words`, `largest_traced`, `largest_work`, `deepest`, `overflows`, `rescans`, `pause_total` and `pause_longest`, and `work` is objects traced plus walked. The free function `reachable` counts the objects the root stack reaches, through the interface alone: right after a collection that is all a heap should hold.

== Mark-sweep and mark-compact: `MarkHeap`

`MarkHeap::new` takes the words, a `Reclaim` (`Sweep` or `Compact`) and the size of the mark stack. The header holds the kind, a `MARK` bit and a `FREE` bit, and no count, so `push`, `pop`, `set_root` and `set_field` store a word and do nothing else.

=== The mark phase

`mark_roots` marks what the root stack refers to, and `mark_some` takes up to a budget of objects off the *mark stack* and marks their fields. The stack has a fixed size. An object that doesn't fit is marked all the same, and the heap is searched afterwards for marked objects with unmarked children:

#srcfile("green-tea-lisp/src/mark.rs", lines: ((165, 197),), notes: ((170, [An object leaves the stack when its fields have been read.]), (183, [A stack that overflowed: nothing records which objects were skipped, so the heap is walked from the start, and `mark` finds them by their unmarked children.]),))[```rust
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
```]

`is_marked` exposes the bit between `mark_roots` and the end of a collection, and `touched` is the words ever used, or in use after a compaction.

=== Sweep

`sweep` walks the heap, clears the marks, and rebuilds both free lists in address order. Nothing moves, so a list of pairs stays where it was. A sweep can't give a pair's three words to a closure, which needs four: a test, `a_sweep_cannot_give_a_pair_s_words_to_a_closure`, pins this down.

=== Compaction

`compact` slides the marked objects to the start of the heap in three passes, and there are no free lists at all:

#srcfile("green-tea-lisp/src/mark.rs", lines: ((241, 255), (272, 294),), notes: ((248, [The first pass writes each object's destination in the top half of its header.]), (273, [The second pass, cut here, rewrites every root and field through `forwarded`. In the third, an object never goes to a higher address, so it overwrites only garbage or words already moved.]),))[```rust
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
// …
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
```]

The `gc` kernel shows where a list ends up under each policy:

#output(title: "cargo xtask kernel --kernel gc --target riscv64")[
```
a list of 4 pairs, made after 4 pairs of garbage, in a heap of 64 words: where its pairs are
  mark-sweep    before: 22 19 16 13; after: 22 19 16 13; the next pair allocated is at 1
  mark-compact  before: 22 19 16 13; after: 10 7 4 1; the next pair allocated is at 13
  copying       before: 21 18 15 12; after: 32 35 38 41; the next pair allocated is at 44
the mark stack:
  a list of 10000 integers: 10000 objects marked, at most 1 on the stack
  a list of 500 lists, a stack of 1024: 2000 objects marked, at most 500 on the stack
  a list of 500 lists, a stack of   16: 2000 objects marked, at most 16 on the stack
    31 objects didn't fit; the heap was searched 31 times, and 62000 objects passed over
```
]

== Copying: `Semispace`

`Semispace::new` takes the words, and divides them into two halves, one in use. `alloc` bumps a pointer through it, with no free list. When the half is full, a collection copies what is reachable into the other half, and the halves swap. Nothing is done about the rest: its half is abandoned as it is.

The traversal is C. J. Cheney's. An object that has been copied has, in its old header, the index of its copy, and the copies themselves are the list of objects still to be read:

#srcfile("green-tea-lisp/src/semispace.rs", lines: ((97, 111),))[```rust
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
```]

#srcfile("green-tea-lisp/src/semispace.rs", lines: ((117, 141),), notes: ((120, [Every root is forwarded, copying what it refers to.]), (125, [`scan` chases `next`. No stack and no queue is kept beside the heap, and the order is breadth-first.]), (133, [Everything not copied is garbage, and the collection never touched it.]),))[```rust
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
```]

The cost of a collection is in proportion to what is *live*, and the price is half the memory. `used` reports the words in the current half. The example `heaps` measures both: the smallest heap each program runs in, in words, found by trying every size:

#output(title: "cargo run --release -p green-tea-lisp --example heaps")[
```
the smallest heap each program computes its value in, in words:
  program   counting  with trial deletion  mark-sweep  mark-compact  copying
  list           614                 1398        1278           614     1228
  fib            143                  143         149           134      268
  alist          811                 1655        1459           766     1532
  ring          2772                  436         429           396      792
  closure       1326                  243         264           237      474
```
]

Copying needs exactly twice what compaction does. Counting ties with compaction on `list`, and needs seven times what compaction needs on `ring`, whose cycles it never frees. Trial deletion's candidate list holds on to dead objects until a search, which is why it needs more for `list`. Heaps with a free list per size aren't monotonic either: some sizes *above* the smallest fail:

#output(title: "the same run")[
```
sizes above the smallest, and less than 400 words above it, where the program doesn't fit:
  list     mark-sweep, smallest 1278: [1279, 1280, 1281, 1283, 1284, 1285]
  ring     with trial deletion, smallest 436: [437, 438, 439, 441, 442, 443]
```
]

One collection, timed on the host, shows what each heap's work depends on. Copying walks nothing and moves what is live. Mark-compact walks the heap three times:

#output(title: "the same run; the last column is a time, and varies from run to run")[
```
  heap             live  garbage   traced   walked  words moved  microseconds
  mark-sweep       1000   100000     1000   101000            0           273
  mark-compact     1000   100000     1000   303000         3000           367
  copying          1000   100000     1000        0         3000            14
  mark-sweep     100000     1000   100000   101000            0           753
  mark-compact   100000     1000   100000   303000       300000          1349
  copying        100000     1000   100000        0       300000           722
```
]

== Two generations: `GenHeap`

=== Layout and collections

The old space starts at word 1 and grows up. The *nursery* is at the top of the array, and every object is born in it. `GenHeap::new` takes the words, the size of the nursery and a `Barrier`. The nursery is that size, or half of the words the old space isn't using if that is less, which is Andrew Appel's arrangement: the other half is where survivors go, so a minor collection can't run out of room.

#layers(caption: [The array of a `GenHeap`, from the top],
  ("nursery", [every new object, from `young` to the end of the array], "amber"),
  ("free", [at least as large as the nursery: where a minor collection copies], "light"),
  ("old space", [words `1..old_top`: survivors, compacted by a major collection], "dark"),
)

A *minor collection* copies what can be reached in the nursery to the end of the old space, with Cheney's scan. It doesn't trace the old space. It starts from the root stack and the *remembered set*, the old objects that may hold a reference to a nursery object:

#srcfile("green-tea-lisp/src/generational.rs", lines: ((263, 269), (274, 306),), notes: ((276, [With `Barrier::Every` the set can name nursery objects. The scan reaches those if they are live.]), (283, [Only fields that refer into the nursery are rewritten.]), (292, [Cheney's scan over the copies.]), (306, [Everything is in the old space now, so no old object refers to the nursery, and the set can be emptied.]),))[```rust
let to = self.old_top;
let mut next = to;
self.counts.roots += self.roots.len() as u64;
self.generations.minor_roots += self.roots.len() as u64;
for slot in 0..self.roots.len() {
    self.roots[slot] = Value::from_bits(self.promote(self.roots[slot].bits(), &mut next));
}
// …
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
```]

A *major collection* runs after a minor one that leaves the old space with more than half of the heap. It marks and slides as `MarkHeap` does when it compacts, with a mark stack that grows as needed, and `place_nursery` then gives the nursery the words it freed.

=== The write barrier

An object can come to hold a reference to a younger one in one way only, a store after it was made. So `set_field` has a barrier, and `alloc` has none:

#srcfile("green-tea-lisp/src/generational.rs", lines: ((567, 594),), notes: ((575, [`Barrier::Filter` is David Ungar's: three tests, and a header bit so that an object is remembered once.]), (590, [`Barrier::Every` is Appel's: test nothing and record every store. The minor collection sorts it out.]),))[```rust
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
```]

The `gen` kernel shows a store creating an old-to-young reference, and the minor collection using it:

#output(title: "cargo xtask kernel --kernel gen --target riscv64")[
```
> (set-cdr! keep (cons 3 nil))
nil
  allocated  7; minor collections 1, objects copied  1; stores recorded 1, remembered set 1
  keep is at word   14, in the old space, and its second pair at word 4048, in the nursery
> (count 20)
0
  allocated 23; minor collections 1, objects copied  2; stores recorded 0, remembered set 0
  keep is at word   14, in the old space, and its second pair at word   75, in the old space
```
]

`store_without_barrier` is there to show what the barrier is for, and the run shows it. A pair made in the nursery and stored in an old pair without the barrier isn't found by the minor collection, and the field ends up naming another object:

#output(title: "the same run", size: 6.8pt)[
```
the pair (1 . 2), made in the nursery and stored in an old pair; after a minor collection and an allocation:
  stored by set_field, the field refers to the pair (1 . 2)
  stored without the barrier, it refers to the pair (3 . 4)
```
]

The other methods report. `generations` returns the `Generations` counters; `is_young`, `old_words`, `nursery_words` and `remembered` describe the layout; `minor` and `major` run a collection directly; and `check` verifies that both areas are objects end to end, every reference names an object, and every old object that refers to the nursery is in the remembered set. `Generations` counts `minor`, `major`, `promoted`, `promoted_words`, `largest_minor`, `minor_roots`, `marked`, `largest_major`, `old_freed`, `deepest`, `stores`, `stores_old`, `old_to_young`, `recorded`, `remembered_most`, `remembered_read`, `remembered_scanned`, `remembered_found`, and the minor and major pause totals and longest.

=== What generations buy

The example `generations` measures the weak generational hypothesis on these programs. With a nursery of 512 words, between 5% and 24% of the objects are alive at their first collection:

#output(title: "cargo run --release -p green-tea-lisp --example generations")[
```
objects copied out of the nursery, in a heap of 1048576 words, by the size of the nursery in words:
  program   allocated     32     64    128    256    512   1024   2048   4096
  list           8190   4613   3254   2660   2284   1964   1639   1117    586
  fib            2002    652    429    281    188    118     75     46     33
  alist         14309  10280   8801   7149   5092   3077   1825   1162    698
  ring           9130   4670   3002   2104   1624   1010    547    336    212
  closure        2877   1120    820    599    374    208    130     99     81
  counters       5002   1910    862    546    347    252    189    154    136
  slots          5499   2662   1773   1381   1131   1003    917    631    377
the same, as a percentage of the objects allocated:
  list                  56.3   39.7   32.5   27.9   24.0   20.0   13.6    7.2
  fib                   32.6   21.4   14.0    9.4    5.9    3.7    2.3    1.6
  alist                 71.8   61.5   50.0   35.6   21.5   12.8    8.1    4.9
  ring                  51.2   32.9   23.0   17.8   11.1    6.0    3.7    2.3
  closure               38.9   28.5   20.8   13.0    7.2    4.5    3.4    2.8
  counters              38.2   17.2   10.9    6.9    5.0    3.8    3.1    2.7
  slots                 48.4   32.2   25.1   20.6   18.2   16.7   11.5    6.9
```
]

`counters` stores 250 integers into old pairs and records nothing. `slots` makes 250 old-to-young references, all of which a minor collection has to find:

#page(flipped: true)[#output(title: "the same run", size: 6.6pt)[
```
the write barrier, in 4096 words with a nursery of 256:
  program   allocated  stores  into old  old to young  recorded  most remembered  objects scanned  references found  recorded by the barrier that tests nothing
  ring           9130     100        48            30        30                1               30                30        100
  closure        2877     100         0             0         0                0                0                 0        100
  counters       5002     250       250             0         0                0                0                 0        250
  slots          5499     250       250           250       250                5              250               250        250
```
]]

A minor collection doesn't depend on the size of the old space. With 100,000 objects kept, it takes tens of nanoseconds, and a collection of the whole heap more than a millisecond:

#output(title: "the same run; the last column is a time", size: 6.5pt)[
```
one collection with a list that is kept and a nursery of 1024 words of garbage, run 1; the last column is a time:
  kept objects  collection          traced   walked  words moved  nanoseconds
        100000  minor                    0        0            0           64
        100000  minor and major     100000   300000            0      1238014
        100000  copying             100000        0       300000      1483291
```
]

The example `store_cost` counts what the barrier costs in instructions, under `valgrind --tool=callgrind`: a million stores into one pair, minus a run with none. A store costs 48 instructions in a heap with no barrier. With the `Filter` barrier it costs 61 to 69, by case, and with `Every` 62 to 68, plus the work of a remembered set a million entries long at the next collection:

#output(title: "valgrind --tool=callgrind target/release/examples/store_cost store <case> <count>; instructions")[
```
plain 0 389494
plain 1000000 48389584
old-integer 0 390121
old-integer 1000000 61390238
old-young 0 390279
old-young 1000000 69390716
every-old-young 0 390298
every-old-young 1000000 62460954
```
]

== Incremental: `IncHeap`

=== Three colours and a barrier

`IncHeap` marks and sweeps in steps, between the steps of the program. The memory, header and mark phase are `MarkHeap`'s, made safe to interleave. A *cycle* does a few *units* at a time, where a unit is one root slot read, one object taken off the mark stack and read, or one object passed over by the sweep.

In the three colours of Dijkstra and his colleagues, an object is *white* if unmarked, *grey* if marked and waiting on the mark stack, and *black* if marked and read. Marking ends when nothing is grey, and the sweep frees what is white. That is only right if no reachable object is white at that moment, and a program that stores a reference to a white object in a black one, and then loses every other reference to it, makes one. `Colour` names the three, and `colour` reports an object's. `Phase` is `Idle`, `Mark` or `Sweep`, reported by `phase`.

`shade` is the primitive: a white object turns grey. The barrier is in `set_field`:

#srcfile("green-tea-lisp/src/incremental.rs", lines: ((683, 698),), notes: ((692, [`Barrier::Insertion` (Dijkstra's) shades the object being stored.]), (693, [`Barrier::Deletion` (Yuasa's, a snapshot at the beginning) shades the object being overwritten.]),))[```rust
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
```]

`Barrier::None` is there to show what goes wrong. The `incremental` kernel builds the bad case on three pairs:

#output(title: "cargo xtask kernel --kernel incremental --target riscv64")[
```
three pairs: B refers to C, which is (1 . 2), and A to nothing; a cycle starts, and takes one step
  no barrier:
    after the step:                A black, B grey, C white
    the program stores C in A:     A black, B grey, C white
    and overwrites it in B:        A black, B grey, C white
    the cycle ends:                C is freed; one more pair is allocated, and A refers to (3 . 4)
  insertion:
    after the step:                A black, B grey, C white
    the program stores C in A:     A black, B grey, C grey
    and overwrites it in B:        A black, B grey, C grey
    the cycle ends:                C is kept; the barrier ran 2 times and shaded 1 object
  deletion:
    after the step:                A black, B grey, C white
    the program stores C in A:     A black, B grey, C white
    and overwrites it in B:        A black, B grey, C grey
    the cycle ends:                C is kept; the barrier ran 2 times and shaded 1 object
```
]

=== Roots and newborns

The root stack has no barrier, because `push`, `pop` and `set_root` are plain stores. Every slot is shaded when a cycle starts. With the insertion barrier, the slots are shaded again each time the mark stack runs empty, and marking ends when that finds nothing new. With the deletion barrier, one scan is enough, because everything reachable at the start gets marked whatever the program does afterwards.

`Birth` is the colour of an object allocated during marking, set by `born`. Only the insertion barrier has the choice. A `White` newborn that dies is freed by the cycle it was born in, and one that lives must be found by a rescan. A `Black` newborn has its fields shaded, since they came off the root stack. During the sweep, an object at an address the sweep hasn't reached is born marked, or the sweep would free it:

#srcfile("green-tea-lisp/src/incremental.rs", lines: ((653, 664),))[```rust
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
```]

=== Steps

`work` runs units until a budget or a time slice is spent:

#srcfile("green-tea-lisp/src/incremental.rs", lines: ((457, 483),), notes: ((468, [An overflow of the mark stack, handled one object per unit by `search_one`, which can be left and resumed.]), (471, [The insertion barrier's rescan of the roots. A scan is never cut short, so a step can go over its budget by the number of root slots.]), (477, [The sweep, one object per unit.]),))[```rust
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
```]

`Pace` is when the steps happen: `Manual` (only `start` and `step`), `Whole` (a full cycle at the threshold), `Alloc(n)` (n units at every allocation), `Tick(n)` (n units at an allocation that finds a timer tick pending), and `Slice(t)` (work for t ticks of the clock). `set_ticker` supplies the poll for the last two.

A cycle starts when `alloc` finds room for fewer than a *reserve* of objects, set by `reserving`, by default one object for every eight words. If `alloc` finds no room at all, it finishes the cycle on the spot and, if that freed nothing of the size, runs another: a *stall*, the pause this heap exists to avoid.

#srcfile("green-tea-lisp/src/incremental.rs", lines: ((554, 567),))[```rust
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
```]

`set_audit` checks each cycle, when marking ends, against a walk from the root stack that ignores the marks, and counts `floating` garbage (marked and unreachable) and `lost` objects (reachable and unmarked). `set_strict` panics at the first lost object, before the sweep frees it. `is_allocated` says whether an index is an allocated object. `steps` returns the `Steps`: `steps`, `units`, `largest`, `stalls`, `largest_stall`, `scans`, `rescans`, `most_roots`, `stores`, `barriers`, `shaded`, `born_marked`, `floating`, `largest_floating`, `lost`, and `pauses`, a histogram of `PAUSE_BUCKETS` (24) powers of two.

=== What it buys

The largest step falls with the pace, and a pace too slow for the program stalls:

#output(title: "cargo run --release -p green-tea-lisp --example incremental")[
```
alist, deletion barrier, a reserve of 512 objects: the pace, in units at each allocation
   units  cycles   steps  most units in a step  stalls  most units in a stall  kept as garbage
       1      32   13581                    59      18                   1915             3775
       2      23   13741                    68       0                      0             2530
       4      38    8417                    63       0                      0             2135
      16      38    1993                    75       0                      0              505
      64      38     507                    75       0                      0               52
     256      38     152                   256       0                      0                0
```
]

The largest step stays near 60 units until the pace passes it, because a scan of the root stack isn't cut short, and `alist` reaches 63 root slots. The floating garbage goes the other way: the longer a cycle is spread out, the more dies during it, and the deletion barrier keeps all of that until the next cycle.

The three ways of keeping the invariant differ in how often they scan the roots. Born white, the insertion barrier rescans thousands of times. On `list`, at one unit per allocation, it finishes no cycle at all, because the program creates white objects as fast as marking can find them:

#output(title: "the same run")[
```
  barrier and birth   units  cycles finished  scans of the root stack  root slots read  marked in them
  insertion, white        1                0                     1859            40437               0
  insertion, white        4               19                     3094            73276            6063
  insertion, black        1                8                       18              308             762
  insertion, black        4               57                      116             2333            8310
  deletion                1                9                       10              166             809
  deletion                4               58                       59             1154            8355
```
]

Without a barrier, a program that changes its objects during marking loses them. `nreverse` has 151 reachable objects freed, and the heap's check stops it at the first use:

#output(title: "the same run")[
```
no barrier, 4 units an allocation:
  program   reachable objects freed  what the program did
  list                            0  computed 50500
  ring                            0  computed 7200
  nreverse                      151  stopped: a freed object was used: 1590
  queue                           0  computed 125250
```
]

== How it is exercised

The tests are in four files: `tests` for the language and `RcHeap`, `gc_tests` for the tracing heaps, `generational_tests` and `incremental_tests`. Some are written once and instantiated by a macro for each heap. They include:

- `every_program_computes_its_value_on_every_heap`
- `each_program_fits_in_its_smallest_heap_and_not_in_a_word_less`
- `after_a_search_everything_allocated_is_reachable`
- `cycles_interleaved_with_a_random_program_lose_nothing`
- `a_list_reversed_during_marking_without_a_barrier_is_freed`, a failure on purpose
- `a_store_without_the_barrier_leaves_a_reference_to_another_object`, another

#output(title: "cargo test -p green-tea-lisp")[
```
test result: ok. 161 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The four examples run on the host. `heaps` finds the smallest heap for each program and times one collection, `generations` measures lifetimes, barriers and pauses, `incremental` measures pace, barriers and floating garbage, and `store_cost` does the work for an instruction counter and prints nothing. The four kernels, `lisp`, `gc`, `gen` and `incremental`, run the same programs in QEMU, and each ends with a line that summarizes it:

#output(title: "the last line of the gc kernel", size: 7.0pt)[
```
PASS: five programs on three heaps that trace: 160 collections traced 25698 objects, and nothing was left
```
]

== Limits

- The language has no strings, floats, macros, `let`, variadic functions or dotted-pair syntax. `define` always binds globally, and the names of the special forms can't be rebound.
- Calls that aren't in tail position nest at most 32 deep, and lists at most 33 deep in the text.
- `Obj` is 32 bits, so a heap holds at most $2^32$ words.
- The sweeping heaps and `RcHeap` keep one free list per object size and never split or merge, so a heap can be full of pairs' words and refuse a closure.
- `Freeing::Recursive` uses Rust stack in proportion to the longest chain that dies: 288 bytes a pair in the `lisp` kernel.
- A `GenHeap` given a nursery smaller than an object can't allocate at all: a heap of 4,096 words with a nursery of 2 returned `HeapFull` on `(cons 1 2)`.
- *Defect.* `lib.rs:9-12` says two heaps can take the place of `RcHeap`, naming `MarkHeap` and `Semispace`. `GenHeap` and `IncHeap` implement the same trait, and the comment predates them.
- *Defect.* `programs/queue.lisp:2-3` says never more than eight numbers wait. `feed` enqueues before it dequeues, so nine do: the same program with a counter reports 9.
- *Defect.* The reader wraps integers that don't fit in 61 bits without an error (`read.rs:101-102`, `lib.rs:117`). `1152921504606846976`, which is $2^60$, reads as $-2^60$.
- *Defect.* `(if #t)`, an `if` with no branches, evaluates to `nil` where a short form is otherwise a `Syntax` error (`eval.rs:258`).
- *Defect.* `print` writes a pair `(1 . 2)` and the three-element list read from the text `(1 . 2)`, whose middle element is the symbol `.`, identically (`print.rs:51`, `read.rs:97-106`). The printer's output for an improper list doesn't read back as the same value.
- *Defect.* `Generations::stores_old` and `old_to_young` are counted only by `Barrier::Filter` (`generational.rs:578-582`). Under `Barrier::Every` they stay 0, though their documentation is unconditional: one old-to-young store reports `stores_old: 0`.
- *Defect.* `Steps::pauses` is documented as how long the steps took (`incremental.rs:133-135`), and also receives the `work` calls of a stall, which aren't steps. A run with 0 steps and 9 stalls had 9 pauses recorded. A stall that runs two `work` calls records two pauses, so `pause_longest` understates it.
- *Defect.* `gc::Counts::walked` is documented as one object per sweep and three per compaction (`gc.rs:29-30`). It also includes the objects passed over when a mark stack overflows (`mark.rs:188`).

One language, six heaps, and the numbers to compare them. @crate-io starts the drivers.
