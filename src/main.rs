use stack::Stack;
use std::arch::global_asm;
use tracing::{Level, info};
use crate::context::{spawn, yield_now, Context, Task, swap_context};

mod stack;
mod context;

global_asm!(include_str!("context.s"));

fn main() {
    static mut MAIN: Context = unsafe { std::mem::zeroed() }; // the scheduler's context
    static mut CURRENT: *mut Task = std::ptr::null_mut();     // the task running right now

    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    let mut tasks = [
        spawn(|| for i in 1..=3 { println!("counter: {i}"); yield_now(); }),
        spawn(|| for w in ["ping", "pong"] { println!("words: {w}"); yield_now(); }),
    ];
    while tasks.iter().any(|t| !t.done) {
        for task in tasks.iter_mut().filter(|t| !t.done) {
            unsafe {
                CURRENT = &raw mut **task;
                swap_context(&raw mut MAIN, &raw const task.context);
            }
        }
    }
    println!("main: all tasks finished");
}



