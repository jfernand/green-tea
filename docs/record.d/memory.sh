# Host tests, and the doc test that checks a request can't outlive its claim.
cargo test -p green-tea-memory -- --test-threads=1 2>/dev/null | grep -E '^test ' | sed 's/; finished.*//' > "$OUT/test.txt"
