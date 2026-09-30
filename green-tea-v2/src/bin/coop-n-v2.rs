use green_tea_v2::{run, spawn, yield_now};
use tracing::Level;

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    run(|| {
        spawn(|| for i in 1..=3 { println!("counter: {i}"); yield_now(); });
        spawn(|| for w in ["ping", "pong"] { println!("words: {w}"); yield_now(); });
    });
    println!("main: all tasks finished");
}
