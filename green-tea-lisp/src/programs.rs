//! The programs the heaps are measured with. Each is a file under `programs/`, and comes
//! with the value its last expression must have.

pub struct Program {
    pub name: &'static str,
    pub source: &'static str,
    /// The value of the last expression, printed.
    pub value: &'static str,
    /// Whether it leaves cycles behind.
    pub cyclic: bool,
}

pub const PROGRAMS: [Program; 5] = [
    Program { name: "list", source: include_str!("../programs/list.lisp"), value: "50500", cyclic: false },
    Program { name: "fib", source: include_str!("../programs/fib.lisp"), value: "610", cyclic: false },
    Program { name: "alist", source: include_str!("../programs/alist.lisp"), value: "250", cyclic: false },
    Program { name: "ring", source: include_str!("../programs/ring.lisp"), value: "7200", cyclic: true },
    Program { name: "closure", source: include_str!("../programs/closure.lisp"), value: "5500", cyclic: true },
];
