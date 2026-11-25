//! Allocating from several CPUs at once.
//!
//! green-tea-fit's allocators take `&mut self`: one caller at a time. Two things here make
//! one usable from several threads, or several harts:
//!
//! - [`lock::SpinLock`] is mutual exclusion with one atomic flag. A heap inside one is
//!   correct from any number of CPUs, and serves one of them at a time.
//! - [`cache::Cache`] is a CPU's own stock of free blocks, by size class. An allocation
//!   that finds a block there takes no lock. The cache is filled from a
//!   [`cache::Central`], which is the heap and a depot of spare blocks behind one lock,
//!   and gives blocks back to it when it holds too many.
//!
//! Neither knows what a thread is. The caller gives each thread, or each hart, a `Cache`
//! of its own, and hands it the same `SpinLock<Central<_>>`.

#![no_std]

pub mod cache;
pub mod lock;
