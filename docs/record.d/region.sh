cargo test -q -p green-tea-region -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo build -q --release -p green-tea-region --example cost
for way in system fit tlsf region scope; do
    for count in 0 100000; do
        printf '%s %s ' "$way" "$count"
        valgrind --tool=callgrind --callgrind-out-file=/dev/null "$CARGO_TARGET_DIR/release/examples/cost" "$way" "$count" 2>&1 >/dev/null | grep -E 'Collected' | sed 's/.*Collected : //'
    done
done > "$OUT/cost.txt"
kernel region riscv64 region-riscv64.txt
