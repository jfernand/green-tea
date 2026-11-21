cargo test -q -p green-tea-virtio -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
for a in riscv64 aarch64 x86_64; do kernel disk $a disk-$a.txt; done
