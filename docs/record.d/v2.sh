# green-tea-v2: tests in both profiles, the demos, the whole `cargo xtask test` matrix, and
# three behaviours at the edges, from a private probe crate.
cargo test -q -p green-tea-v2 -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo test -q -p green-tea-v2 --release -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test-release.txt"
cargo run -q -p green-tea-v2 --bin coop-2-v2 > "$OUT/coop-2.txt" 2>&1
cargo run -q -p green-tea-v2 --bin coop-n-v2 > "$OUT/coop-n.txt" 2>&1
cargo xtask test 2>&1 | sed -n '/^=== summary/,$p' > "$OUT/xtask-test.txt"

P=$CARGO_TARGET_DIR/probe-v2
mkdir -p "$P/src/bin" "$P/.cargo"
cat > "$P/Cargo.toml" <<EOF
[package]
name = "probe-v2"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
green-tea-v2 = { path = "$SRC/green-tea-v2" }
EOF
cp "$SRC/Cargo.lock" "$P/"
cp "$SRC/.cargo/config.toml" "$P/.cargo/"
cat > "$P/src/bin/nested.rs" <<'EOF'
fn main() {
    green_tea_v2::run(|| {
        println!("a task calls run");
        green_tea_v2::run(|| {});
        println!("after the inner run");
    });
}
EOF
cat > "$P/src/bin/scope.rs" <<'EOF'
use green_tea_v2::raw::{Task, yield_now};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::{thread, time::Duration};
static GO: AtomicBool = AtomicBool::new(false);
fn main() {
    let mut task = Task::new(|| {
        let local = AtomicU64::new(7);
        thread::scope(|s| {
            s.spawn(|| {
                while !GO.load(SeqCst) {
                    thread::yield_now();
                }
                println!("the scoped thread reads the task's local: {}", local.load(SeqCst));
            });
            yield_now();
        });
    });
    task.resume();
    drop(task);
    println!("the task was dropped with its scope open");
    GO.store(true, SeqCst);
    thread::sleep(Duration::from_millis(300));
}
EOF
cat > "$P/src/bin/round.rs" <<'EOF'
use green_tea_v2::{run, spawn, yield_now};
unsafe extern "C" {
    fn fesetround(mode: i32) -> i32;
    fn fegetround() -> i32;
}
#[cfg(target_arch = "x86_64")]
const UPWARD: i32 = 0x800;
#[cfg(target_arch = "aarch64")]
const UPWARD: i32 = 0x400000;
#[cfg(target_arch = "riscv64")]
const UPWARD: i32 = 3;
const ARCH: &str = std::env::consts::ARCH;
fn main() {
    println!("{ARCH}: at the start, rounding mode {}", unsafe { fegetround() });
    run(|| {
        spawn(|| {
            unsafe { fesetround(UPWARD) };
            println!("{ARCH}: task A sets rounding mode {UPWARD}");
            yield_now();
        });
        spawn(|| println!("{ARCH}: task B finds rounding mode {}", unsafe { fegetround() }));
    });
}
EOF
(cd "$P" && cargo build -q --offline
 cargo build -q --offline --bin round --target aarch64-unknown-linux-gnu
 cargo build -q --offline --bin round --target riscv64gc-unknown-linux-gnu)
for b in nested scope; do
    (ulimit -c 0; "$CARGO_TARGET_DIR/debug/$b" 2>&1 | sed -E 's/\(([0-9]+)\) panicked/panicked/; s#at /.*/green-tea-v2#at green-tea-v2#' > "$OUT/probe-$b.txt"; echo "exit status ${PIPESTATUS[0]}" >> "$OUT/probe-$b.txt") 2>/dev/null || true
done
{ "$CARGO_TARGET_DIR/debug/round"
  qemu-aarch64 -L /usr/aarch64-linux-gnu "$CARGO_TARGET_DIR/aarch64-unknown-linux-gnu/debug/round"
  qemu-riscv64 -L /usr/riscv64-linux-gnu "$CARGO_TARGET_DIR/riscv64gc-unknown-linux-gnu/debug/round"
} > "$OUT/probe-round.txt" 2>&1
# The section on green-tea-arch quotes these two as well.
mkdir -p "$OUT/../arch"
cp "$OUT/xtask-test.txt" "$OUT/probe-round.txt" "$OUT/../arch/"
