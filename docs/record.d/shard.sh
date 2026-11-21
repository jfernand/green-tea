cargo test -q -p green-tea-shard -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
kernel shard riscv64 shard-riscv64.txt
