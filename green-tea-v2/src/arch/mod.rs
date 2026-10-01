//! The machine-specific layer: saving and restoring registers, and priming a fresh stack.
//! Each architecture provides the same `Context`, `Context::new`, `Entry` and `swap_context`.

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;

#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
compile_error!("green-tea-v2 supports x86_64, aarch64 and riscv64 so far");
