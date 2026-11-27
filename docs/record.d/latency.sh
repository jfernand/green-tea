# The bin target has test = false: cargo test builds it but runs none of histogram.rs's tests.
cargo test -p green-tea-latency 2>&1 | grep -E '^ *(Finished|running|test result)' | sed -e 's/^ *//' -e 's/ in [0-9.]*s$//' -e 's/; finished.*//' > "$OUT/test.txt"
kernel latency x86_64 latency-x86_64.txt
kernel latency-deadline x86_64 latency-deadline-x86_64.txt
for k in latency latency-deadline; do
    cargo xtask kernel --board pc --kernel $k --target x86_64 --headless >/dev/null 2>&1
    cp "$CARGO_TARGET_DIR/kernel/$k-x86_64.log" "$OUT/$k-pc.txt"
done
