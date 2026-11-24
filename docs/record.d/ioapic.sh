cargo test -q -p green-tea-ioapic -- --test-threads=1 2>/dev/null | grep -E '^test result' | head -1 | sed 's/; finished.*//' > "$OUT/test.txt"
# The pc board's interrupts kernel: bytes at the serial port, each waking the halted shell.
cargo xtask kernel --board pc --kernel interrupts --target x86_64 --headless >/dev/null 2>&1
cp "$CARGO_TARGET_DIR/kernel/interrupts-x86_64.log" "$OUT/interrupts-pc.txt"
