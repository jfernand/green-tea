use green_tea_v2::raw::{Task, yield_now};
use tracing::Level;

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    let mut tasks = [
        Task::new(|| for i in 1..=3 { println!("counter: {i}"); yield_now(); }),
        Task::new(|| for w in ["ping", "pong"] { println!("words: {w}"); yield_now(); }),
    ];
    while tasks.iter().any(|t| !t.is_done()) {
        for task in tasks.iter_mut().filter(|t| !t.is_done()) {
            task.resume();
        }
    }
    println!("main: all tasks finished");
}
