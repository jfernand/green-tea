# Host tests under the default Green profile, then under Black+White, then the two base
# profiles together, which the crate refuses to compile.
cargo test -p green-tea-capabilities -- --test-threads=1 2>/dev/null | grep -E '^test ' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo test -p green-tea-capabilities --no-default-features --features black,white -- --test-threads=1 2>/dev/null | grep -E '^test ' | sed 's/; finished.*//' > "$OUT/test-black-white.txt"
cargo build -p green-tea-capabilities --features black 2>&1 | grep -E '^error' | head -2 > "$OUT/both.txt" || true
