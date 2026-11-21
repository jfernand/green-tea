cargo test -q -p green-tea-pci -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
kernel disk x86_64 disk-x86_64.txt
