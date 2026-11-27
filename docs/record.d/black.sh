# The protected-process kernel on QEMU's q35, then on the pc board in QEMU, where xtask
# types A and B at the emulated PS/2 keyboard from QEMU's monitor.
kernel protected x86_64 protected-x86_64.txt
cargo xtask kernel --board pc --kernel protected --target x86_64 --headless > "$OUT/xtask-pc.txt" 2>&1 || true
cp "$CARGO_TARGET_DIR/kernel/protected-x86_64.log" "$OUT/protected-pc.txt"
