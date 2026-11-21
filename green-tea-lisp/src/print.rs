//! The printer: objects to text.

use crate::eval::Lisp;
use crate::{Heap, Kind, Val, Value};
use core::fmt::{self, Write};

/// The printer stops after this many atoms, so that printing a cycle ends.
const MAX_ATOMS: usize = 64;
/// And nests no deeper than this.
const MAX_NESTING: usize = 16;

impl<H: Heap> Lisp<H> {
    /// Write `v` as the reader would read it, where it can be read.
    pub fn print(&self, v: Value, out: &mut dyn Write) -> fmt::Result {
        self.print_nested(v, out, &mut MAX_ATOMS.clone(), 0)
    }

    fn print_nested(&self, v: Value, out: &mut dyn Write, budget: &mut usize, nesting: usize) -> fmt::Result {
        if *budget == 0 || nesting > MAX_NESTING {
            return out.write_str("...");
        }
        *budget -= 1;
        let pair = match v.decode() {
            Val::Nil => return out.write_str("nil"),
            Val::True => return out.write_str("#t"),
            Val::Int(n) => return write!(out, "{n}"),
            Val::Sym(s) => return out.write_str(self.name(s)),
            Val::Prim(p) => return write!(out, "#<primitive {}>", crate::eval::PRIMITIVES[p as usize]),
            Val::Obj(obj) => match self.heap.kind(obj) {
                Kind::Pair => obj,
                Kind::Closure => return out.write_str("#<function>"),
                Kind::Binding => return out.write_str("#<environment>"),
            },
        };
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
    }
}
