for a in riscv64 aarch64 x86_64; do kernel memmap $a memmap-$a.txt; done
kernel overflow riscv64 overflow-riscv64.txt
kernel overflow x86_64 overflow-x86_64.txt
kernel pages riscv64 pages-riscv64.txt
kernel smp x86_64 smp-x86_64.txt
# Last, because it rebuilds the kernel with the board-pc feature.
cargo xtask kernel --board pc --kernel coop --headless >/dev/null 2>&1
cp "$CARGO_TARGET_DIR/kernel/coop-x86_64.log" "$OUT/pc-coop-x86_64.txt"
