# green-tea-v1: its tests, its two demos, and two misuses that a private probe crate commits.
cargo test -q -p green-tea-v1 -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo run -q -p green-tea-v1 --bin coop-2-v1 > "$OUT/coop-2.txt" 2>&1
cargo run -q -p green-tea-v1 --bin coop-n-v1 > "$OUT/coop-n.txt" 2>&1

P=$CARGO_TARGET_DIR/probe-v1
mkdir -p "$P/src/bin"
cat > "$P/Cargo.toml" <<EOF
[package]
name = "probe-v1"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
green-tea-v1 = { path = "$SRC/green-tea-v1" }
EOF
cp "$SRC/Cargo.lock" "$P/"
cat > "$P/src/bin/nested.rs" <<'EOF'
use green_tea_v1::context::{run, spawn, yield_now};
fn main() {
    run([spawn(|| {
        println!("outer task: before the inner run");
        run([spawn(|| println!("inner task"))]);
        println!("outer task: after the inner run");
        yield_now();
        println!("outer task: resumed");
    })]);
    println!("main: returned");
}
EOF
cat > "$P/src/bin/outside.rs" <<'EOF'
fn main() {
    println!("calling yield_now outside a task");
    green_tea_v1::context::yield_now();
    println!("returned");
}
EOF
(cd "$P" && cargo build -q --offline --manifest-path Cargo.toml)
for b in nested outside; do
    (ulimit -c 0; "$CARGO_TARGET_DIR/debug/$b" > "$OUT/probe-$b.txt" 2>&1; echo "exit status $?" >> "$OUT/probe-$b.txt") 2>/dev/null || true
done
