//! The evaluator.
//!
//! `eval` is called with an expression and an environment in the top two slots of the
//! root stack, and leaves the expression's value in their place. Everything it holds
//! between two operations that may allocate or free is in a root slot: Rust variables
//! hold slot numbers, immediates, and references read since the last such operation.
//!
//! An environment is a chain of [`Kind::Binding`] objects. The global environment is the
//! chain in root slot 0, and `define` always binds there. A call in tail position reuses
//! the caller's two slots, so a loop written as a tail call runs in constant root stack
//! and constant Rust stack.

use crate::read::Text;
use crate::{Full, Heap, Kind, Obj, Val, Value};
use alloc::string::String;
use alloc::vec::Vec;

/// The symbols the evaluator knows by number: the special forms, then the primitives.
pub const QUOTE: u32 = 0;
const IF: u32 = 1;
const DEFINE: u32 = 2;
const LAMBDA: u32 = 3;
const SPECIAL_FORMS: [&str; 4] = ["quote", "if", "define", "lambda"];

/// The primitives. A symbol that no environment binds, and that names one, evaluates to it.
pub const PRIMITIVES: [&str; 13] =
    ["car", "cdr", "cons", "set-car!", "set-cdr!", "+", "-", "*", "remainder", "<", "=", "eq?", "null?"];

/// The root slot that holds the global environment.
const GLOBALS: usize = 0;

/// How deeply `eval` may call itself: calls that aren't in tail position, nested. The
/// kernels run the interpreter on their 64 KiB boot stack, and a debug build spends about
/// 1.5 KiB of it on each level.
pub const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

impl From<Full> for Error {
    fn from(_: Full) -> Error {
        Error::HeapFull
    }
}

/// What `classify` found in an expression.
enum Form {
    /// Its value is already on the root stack.
    Value,
    If,
    Define,
    Call,
}

#[derive(PartialEq)]
enum Applied {
    Primitive,
    Closure,
}

/// An interpreter over a heap of type `H`.
pub struct Lisp<H: Heap> {
    pub heap: H,
    symbols: Vec<String>,
    depth: usize,
    deepest: usize,
}

impl<H: Heap> Lisp<H> {
    /// An interpreter with nothing defined. `heap`'s root stack must be empty.
    pub fn new(mut heap: H) -> Lisp<H> {
        assert_eq!(heap.roots(), 0);
        heap.push(Value::NIL); // the global environment
        let symbols = SPECIAL_FORMS.iter().chain(&PRIMITIVES).map(|&s| String::from(s)).collect();
        Lisp { heap, symbols, depth: 0, deepest: 0 }
    }

    /// The number of the symbol spelled `name`.
    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(n) = self.symbols.iter().position(|s| s == name) {
            return n as u32;
        }
        self.symbols.push(String::from(name));
        self.symbols.len() as u32 - 1
    }

    pub fn name(&self, symbol: u32) -> &str {
        &self.symbols[symbol as usize]
    }

    /// Read and evaluate every expression in `source`, and return the last value, printed.
    /// Whatever happens, the root stack is left holding the global environment alone.
    pub fn run(&mut self, source: &str) -> Result<String, Error> {
        let mut text = Text::new(source);
        let mut printed = String::new();
        let result = self.run_text(&mut text, &mut printed);
        while self.heap.roots() > GLOBALS + 1 {
            self.heap.pop();
        }
        self.depth = 0;
        result.map(|()| printed)
    }

    fn run_text(&mut self, text: &mut Text, printed: &mut String) -> Result<(), Error> {
        while text.more() {
            self.read(text)?;
            self.heap.push(Value::NIL); // no local environment
            self.eval()?;
            if !text.more() {
                let _ = self.print(self.heap.top(), printed);
            }
            self.heap.pop();
        }
        Ok(())
    }

    /// The most calls of `eval` that have been nested.
    pub fn deepest(&self) -> usize {
        self.deepest
    }

    /// The value `name` is defined as. Borrowed.
    pub fn global(&self, name: &str) -> Option<Value> {
        let symbol = self.symbols.iter().position(|s| s == name)? as u32;
        Some(self.heap.field(self.binding(symbol, self.heap.root(GLOBALS))?, 1))
    }

    /// Forget every definition. What the heap still holds afterwards, it can't free.
    pub fn forget_globals(&mut self) {
        self.heap.set_root(GLOBALS, Value::NIL);
    }

    /// Evaluate the expression in the root slot below the top, in the environment on top.
    /// Both are replaced by the value.
    pub fn eval(&mut self) -> Result<(), Error> {
        if self.depth == MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        self.depth += 1;
        self.deepest = self.deepest.max(self.depth);
        let base = self.heap.roots() - 2;
        self.eval_at(base)?; // on an error, `run` empties the root stack
        self.depth -= 1;
        // The value is on top. Put it where the expression was, and drop the rest.
        self.heap.set_root(base, self.heap.top());
        while self.heap.roots() > base + 1 {
            self.heap.pop();
        }
        Ok(())
    }

    /// `eval`, with the expression in slot `base` and the environment in slot `base + 1`.
    /// Pushes the value. A tail call replaces both slots and goes round the loop.
    ///
    /// This function and `eval` are the only two on the stack once for every call that
    /// is nested in another, so everything that needs variables is in a helper that
    /// returns before `eval` is called again.
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

    /// Look at the expression in slot `base`. If it has a value that needs no further
    /// call of `eval` (a constant, a variable, a `quote`, a `lambda`), push the value.
    /// For an `if`, push the test and the environment, ready for `eval`.
    fn classify(&mut self, base: usize) -> Result<Form, Error> {
        let env = base + 1;
        let expr = self.heap.root(base);
        let form = match expr.decode() {
            Val::Sym(symbol) => {
                let value = self.lookup(symbol, self.heap.root(env))?;
                self.heap.push(value);
                return Ok(Form::Value);
            }
            Val::Obj(obj) if self.heap.kind(obj) == Kind::Pair => obj,
            _ => {
                self.heap.push(expr); // a number, nil, #t: its own value
                return Ok(Form::Value);
            }
        };
        match self.heap.field(form, 0).decode() {
            Val::Sym(QUOTE) => {
                let quoted = self.nth(expr, 1)?;
                self.heap.push(quoted);
                Ok(Form::Value)
            }
            Val::Sym(LAMBDA) => {
                let parameters = self.nth(expr, 1)?;
                let body = self.tail(expr, 2)?;
                self.heap.push(parameters);
                self.heap.push(body);
                self.heap.push(self.heap.root(env));
                self.heap.alloc(Kind::Closure)?;
                Ok(Form::Value)
            }
            Val::Sym(IF) => {
                let test = self.nth(expr, 1)?;
                self.heap.push(test);
                self.heap.push(self.heap.root(env));
                Ok(Form::If)
            }
            Val::Sym(DEFINE) => Ok(Form::Define),
            _ => Ok(Form::Call),
        }
    }

    /// The value of an `if`'s test is on top of the root stack. Pop it, and put the
    /// branch it chooses in slot `base`, in place of the `if`.
    fn choose(&mut self, base: usize) {
        let chosen = if self.heap.top() != Value::NIL { 2 } else { 3 };
        self.heap.pop();
        // The `if` is read again from its slot: `eval` has allocated since `classify` read it.
        let branch = self.nth(self.heap.root(base), chosen).unwrap_or(Value::NIL);
        self.heap.set_root(base, branch);
    }

    /// Push element `n` of the call in slot `base`, and the environment, ready for `eval`.
    /// `false` when the call has no element `n`.
    fn operand(&mut self, base: usize, n: usize) -> Result<bool, Error> {
        let rest = self.tail(self.heap.root(base), n)?;
        let Some(pair) = self.pair(rest) else { return Ok(false) };
        self.heap.push(self.heap.field(pair, 0));
        self.heap.push(self.heap.root(base + 1));
        Ok(true)
    }

    /// Call the function in slot `base + 2` with the `arguments` values in the slots after
    /// it. A primitive is run, and its result pushed. For a closure, the callee's body
    /// and environment replace the caller's expression and environment in slots `base`
    /// and `base + 1`, and the function and its arguments are dropped: a tail call.
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

    /// Slot `base` holds what is left of a function's body. If more than one expression
    /// is left, push the first and the environment, ready for `eval`, and leave the rest
    /// in the slot. If one is left, put it in the slot: it is in tail position.
    fn statement(&mut self, base: usize) -> Result<bool, Error> {
        let body = self.pair(self.heap.root(base)).ok_or(Error::Syntax("a function with no body"))?;
        let (first, rest) = (self.heap.field(body, 0), self.heap.field(body, 1));
        if rest == Value::NIL {
            self.heap.set_root(base, first);
            return Ok(false);
        }
        self.heap.push(first);
        self.heap.push(self.heap.root(base + 1));
        self.heap.set_root(base, rest);
        Ok(true)
    }

    /// The first half of `(define name expression)` or `(define (name parameter...)
    /// body...)`. For the second, push the closure. For the first, push the expression
    /// and the environment, and return `true`: the value is one `eval` away.
    fn defined_value(&mut self, base: usize) -> Result<bool, Error> {
        let expr = self.heap.root(base);
        match self.pair(self.nth(expr, 1)?) {
            Some(signature) => {
                self.heap.push(self.heap.field(signature, 1));
                self.heap.push(self.tail(expr, 2)?);
                self.heap.push(self.heap.root(base + 1));
                self.heap.alloc(Kind::Closure)?;
                Ok(false)
            }
            None => {
                self.heap.push(self.nth(expr, 2)?);
                self.heap.push(self.heap.root(base + 1));
                Ok(true)
            }
        }
    }

    /// The second half: the value is on top of the root stack. Bind the name to it in the
    /// global environment, and push the name as the value of the `define`.
    fn define(&mut self, base: usize) -> Result<(), Error> {
        let target = self.nth(self.heap.root(base), 1)?;
        let name = self.pair(target).map_or(target, |signature| self.heap.field(signature, 0));
        let Val::Sym(symbol) = name.decode() else { return Err(Error::Syntax("define needs a name")) };
        let value = self.heap.roots() - 1;
        match self.binding(symbol, self.heap.root(GLOBALS)) {
            Some(binding) => self.heap.set_field(binding, 1, self.heap.root(value)),
            None => {
                self.heap.push(name);
                self.heap.push(self.heap.root(value));
                self.heap.push(self.heap.root(GLOBALS));
                self.heap.alloc(Kind::Binding)?;
                self.heap.set_root(GLOBALS, self.heap.top());
                self.heap.pop();
            }
        }
        self.heap.push(name);
        Ok(())
    }

    /// Apply a primitive to the `count` arguments in root slots `first..`, and push the result.
    fn primitive(&mut self, primitive: u32, first: usize, count: usize) -> Result<(), Error> {
        let name = PRIMITIVES[primitive as usize];
        let expected = if matches!(name, "car" | "cdr" | "null?") { 1 } else { 2 };
        if count != expected {
            return Err(Error::Arguments);
        }
        let a = self.heap.root(first);
        let b = if count == 2 { self.heap.root(first + 1) } else { Value::NIL };
        let truth = |t: bool| if t { Value::TRUE } else { Value::NIL };
        let result = match name {
            "car" => self.heap.field(self.pair(a).ok_or(Error::NotAPair)?, 0),
            "cdr" => self.heap.field(self.pair(a).ok_or(Error::NotAPair)?, 1),
            "cons" => {
                self.heap.push(a);
                self.heap.push(b);
                self.heap.alloc(Kind::Pair)?;
                return Ok(());
            }
            "set-car!" | "set-cdr!" => {
                let pair = self.pair(a).ok_or(Error::NotAPair)?;
                self.heap.set_field(pair, usize::from(name == "set-cdr!"), b);
                Value::NIL
            }
            "eq?" => truth(a == b),
            "null?" => truth(a == Value::NIL),
            _ => {
                let (Val::Int(a), Val::Int(b)) = (a.decode(), b.decode()) else { return Err(Error::NotANumber) };
                match name {
                    "+" => Value::int(a.wrapping_add(b)),
                    "-" => Value::int(a.wrapping_sub(b)),
                    "*" => Value::int(a.wrapping_mul(b)),
                    "remainder" if b == 0 => return Err(Error::DivisionByZero),
                    "remainder" => Value::int(a.wrapping_rem(b)),
                    "<" => truth(a < b),
                    _ => truth(a == b),
                }
            }
        };
        self.heap.push(result);
        Ok(())
    }

    /// The value of a symbol: from the environment, then from the global environment,
    /// and then as the name of a primitive.
    fn lookup(&self, symbol: u32, env: Value) -> Result<Value, Error> {
        if let Some(binding) = self.binding(symbol, env).or_else(|| self.binding(symbol, self.heap.root(GLOBALS))) {
            return Ok(self.heap.field(binding, 1));
        }
        let primitive = symbol.wrapping_sub(SPECIAL_FORMS.len() as u32);
        if (primitive as usize) < PRIMITIVES.len() { Ok(Value::prim(primitive)) } else { Err(Error::Unbound(symbol)) }
    }

    /// The binding of `symbol` in the chain that starts at `env`.
    fn binding(&self, symbol: u32, mut env: Value) -> Option<Obj> {
        while let Some(binding) = env.as_obj() {
            if self.heap.field(binding, 0) == Value::sym(symbol) {
                return Some(binding);
            }
            env = self.heap.field(binding, 2);
        }
        None
    }

    fn pair(&self, v: Value) -> Option<Obj> {
        v.as_obj().filter(|&obj| self.heap.kind(obj) == Kind::Pair)
    }

    /// What is left of a list after its first `n` elements.
    fn tail(&self, mut list: Value, n: usize) -> Result<Value, Error> {
        for _ in 0..n {
            list = self.heap.field(self.pair(list).ok_or(Error::Syntax("a form is too short"))?, 1);
        }
        Ok(list)
    }

    /// Element `n` of a list, counting from 0.
    fn nth(&self, list: Value, n: usize) -> Result<Value, Error> {
        let rest = self.tail(list, n)?;
        Ok(self.heap.field(self.pair(rest).ok_or(Error::Syntax("a form is too short"))?, 0))
    }
}
