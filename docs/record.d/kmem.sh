cargo test -q -p green-tea-kmem -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
kernel slab riscv64 slab-riscv64.txt
