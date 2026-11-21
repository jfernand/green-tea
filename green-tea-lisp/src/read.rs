//! The reader: text to objects.
//!
//! A list is read by pushing each of its elements on the root stack, then `nil`, and then
//! allocating one pair for each element, last element first: `alloc` pops a `car` and a
//! `cdr` and pushes the pair, which is the `cdr` the next `alloc` needs. In `(a b . c)`,
//! `c` takes the place of `nil`.

use crate::eval::{Error, Lisp, QUOTE};
use crate::{Heap, Kind, Value};
use core::num::IntErrorKind;

/// How deeply lists may nest in the text.
const MAX_NESTING: usize = 32;

/// A position in the text being read.
pub struct Text<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Text<'a> {
    pub fn new(source: &'a str) -> Text<'a> {
        Text { bytes: source.as_bytes(), at: 0 }
    }

    /// The next byte that isn't blank or in a comment (`;` to the end of the line).
    fn peek(&mut self) -> Option<u8> {
        loop {
            match self.bytes.get(self.at).copied() {
                Some(b';') => {
                    while self.bytes.get(self.at).is_some_and(|&b| b != b'\n') {
                        self.at += 1;
                    }
                }
                Some(b) if b.is_ascii_whitespace() => self.at += 1,
                other => return other,
            }
        }
    }

    /// Whether the next byte is a `.` standing alone, and not the start of a symbol.
    fn at_dot(&self) -> bool {
        self.bytes[self.at] == b'.'
            && self.bytes.get(self.at + 1).is_none_or(|&b| b.is_ascii_whitespace() || b"()';".contains(&b))
    }

    /// Whether anything is left to read.
    pub fn more(&mut self) -> bool {
        self.peek().is_some()
    }
}

impl<H: Heap> Lisp<H> {
    /// Read one expression, and leave it on top of the root stack.
    pub fn read(&mut self, text: &mut Text) -> Result<(), Error> {
        self.read_nested(text, 0)
    }

    fn read_nested(&mut self, text: &mut Text, nesting: usize) -> Result<(), Error> {
        if nesting > MAX_NESTING {
            return Err(Error::Syntax("lists nested too deeply"));
        }
        match text.peek().ok_or(Error::Syntax("the text ended inside an expression"))? {
            b'(' => {
                text.at += 1;
                let (mut elements, mut dotted) = (0, false);
                loop {
                    match text.peek() {
                        None => return Err(Error::Syntax("a list without its )")),
                        Some(b')') => break,
                        Some(b'.') if text.at_dot() => {
                            if elements == 0 {
                                return Err(Error::Syntax("a . with no element before it"));
                            }
                            text.at += 1;
                            if matches!(text.peek(), None | Some(b')')) {
                                return Err(Error::Syntax("a . with nothing after it"));
                            }
                            // What follows the dot is the last pair's `cdr`, in place of `nil`.
                            self.read_nested(text, nesting + 1)?;
                            if text.peek() != Some(b')') {
                                return Err(Error::Syntax("more than one value after a ."));
                            }
                            dotted = true;
                            break;
                        }
                        Some(_) => self.read_nested(text, nesting + 1)?,
                    }
                    elements += 1;
                }
                text.at += 1;
                if !dotted {
                    self.heap.push(Value::NIL);
                }
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
                let atom = self.atom(atom)?;
                self.heap.push(atom);
                Ok(())
            }
        }
    }

    fn atom(&mut self, text: &str) -> Result<Value, Error> {
        const TOO_LARGE: Error = Error::Syntax("an integer outside the 61 bits of a value");
        Ok(match text {
            "nil" => Value::NIL,
            "#t" => Value::TRUE,
            _ => match text.parse::<i64>() {
                Ok(n) if (Value::MIN_INT..=Value::MAX_INT).contains(&n) => Value::int(n),
                Ok(_) => return Err(TOO_LARGE),
                Err(e) if matches!(e.kind(), IntErrorKind::PosOverflow | IntErrorKind::NegOverflow) => {
                    return Err(TOO_LARGE);
                }
                Err(_) => Value::sym(self.intern(text)),
            },
        })
    }
}
