use green_tea_v1::context::{CURRENT, MAIN, spawn, swap_context, yield_now};
use tracing::Level;

fn main() {
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
