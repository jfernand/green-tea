cargo test -q -p green-tea-smr -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
kernel smr riscv64 smr-riscv64.txt
