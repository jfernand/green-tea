for a in riscv64 aarch64 x86_64; do kernel rt $a rt-$a.txt; done
# The pc board in QEMU (UEFI firmware, the board the laptop boots).
cargo xtask kernel --board pc --kernel rt --target x86_64 --headless >/dev/null 2>&1
cp "$CARGO_TARGET_DIR/kernel/rt-x86_64.log" "$OUT/rt-pc.txt"
