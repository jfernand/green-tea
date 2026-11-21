cargo test -q -p green-tea-framebuffer -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo xtask kernel --board pc --kernel shell --target x86_64 --headless >/dev/null 2>&1
cp "$CARGO_TARGET_DIR/kernel/shell-x86_64.log" "$OUT/shell-pc.txt"
