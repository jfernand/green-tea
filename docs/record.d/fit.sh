cargo test -q -p green-tea-fit -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo run -q -p green-tea-fit --example interface > "$OUT/interface.txt" 2>&1
for k in fit frag tlsf safety; do kernel $k riscv64 $k-riscv64.txt; done
