//! Cooperative tasks and a round-robin runtime for green-tea's bare-metal kernels.
//!
//! The runtime depends on `green-tea-bare` for platform services, task stacks, and task
//! record allocation; the bare-metal crate does not depend on this scheduler.

#![no_std]

extern crate alloc;

pub mod console;
pub mod runtime;
pub mod task;
