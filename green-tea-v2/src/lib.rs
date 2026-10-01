//! Green threads on x86_64, aarch64 (e.g. Apple Silicon) and riscv64, in three layers:
//!
//! - [`arch`](raw): save/restore registers and prime a fresh stack (the mechanism).
//! - [`Task`](raw::Task): an asymmetric coroutine with its own stack (resume / yield).
//! - [`run`] / [`spawn`]: a cooperative round-robin scheduler (the policy).

mod arch;
mod runtime;
mod stack;
mod task;

pub use runtime::{run, spawn};
pub use task::yield_now;

/// The lower layers, for driving tasks by hand instead of through [`run`].
pub mod raw {
    pub use crate::arch::{Context, Entry, swap_context};
    pub use crate::task::{Task, yield_now};
}
