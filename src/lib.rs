use std::arch::global_asm;

pub mod context;
mod stack;

global_asm!(include_str!("context.s"));
