cargo test -q -p green-tea-rt -- --test-threads=1 2>/dev/null | grep -E '^test result' | head -1 | sed 's/; finished.*//' > "$OUT/test.txt"
