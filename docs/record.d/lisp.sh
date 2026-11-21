cargo test -q -p green-tea-lisp -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo run -q --release -p green-tea-lisp --example heaps > "$OUT/heaps.txt" 2>&1
cargo run -q --release -p green-tea-lisp --example generations > "$OUT/generations.txt" 2>&1
cargo run -q --release -p green-tea-lisp --example incremental > "$OUT/incremental.txt" 2>&1
cargo build -q --release -p green-tea-lisp --example store_cost
for c in plain young old-integer old-old old-young every-young every-old-integer every-old-old every-old-young; do
    for n in 0 1000000; do
        printf '%s %s ' "$c" "$n"
        valgrind --tool=callgrind --callgrind-out-file=/dev/null "$CARGO_TARGET_DIR/release/examples/store_cost" store "$c" "$n" 2>&1 >/dev/null | grep -E 'Collected' | sed 's/.*Collected : //'
    done
done > "$OUT/store-cost.txt"
for k in lisp gc gen incremental; do kernel $k riscv64 $k-riscv64.txt; done
