//! The machine-specific layer: saving and restoring registers, and priming a fresh stack.

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(not(target_arch = "x86_64"))]
compile_error!("green-tea-v2 only supports x86_64 so far");
