cargo test -q -p green-tea-pool -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo run -q -p green-tea-pool --example interface > "$OUT/interface.txt" 2>&1
kernel pool riscv64 kernel-riscv64.txt
