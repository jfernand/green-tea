//! Measuring the allocators of this workspace on recorded traces.
//!
//! - [`subjects`]: every single-threaded allocator behind one interface.
//! - [`replay`]: a trace made ready to replay, and replayed for bytes, for instructions
//!   and against a clock.
//! - [`stats`], which has the percentiles.
//! - [`oracle`]: explicit freeing for the Lisp's programs, from lifetimes found in an
//!   earlier run.
//!
//! `main.rs` is the command that prints the tables.

pub mod oracle;
pub mod replay;
pub mod stats;
pub mod subjects;

#[cfg(test)]
mod tests;
