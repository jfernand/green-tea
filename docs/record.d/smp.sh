cargo test -q -p green-tea-smp -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
kernel smp riscv64 smp-riscv64.txt
