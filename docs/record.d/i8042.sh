cargo test -q -p green-tea-i8042 -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
# Boots the pc board's shell in QEMU with OVMF, and types at the emulated keyboard from
# QEMU's monitor, so that the characters reach the kernel only through this driver.
cargo xtask kernel --board pc --kernel shell --target x86_64 --headless >/dev/null 2>&1
pc=$CARGO_TARGET_DIR/kernel/pc
log=$OUT/typed-pc.txt
rm -f "$log"
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$pc/typed.vars"
{
    for _ in $(seq 600); do grep -q '^> ' "$log" 2>/dev/null && break; sleep 0.1; done
    sleep 1
    for key in e c h o spc k e y s ret; do echo "sendkey $key"; sleep 0.3; done
    sleep 1
    echo quit
} | timeout 120 qemu-system-x86_64 -machine q35 -cpu max -m 512M -vga std -net none  \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$pc/typed.vars" \
    -drive if=none,id=esp,format=raw,readonly=on,file=fat:"$pc/shell-x86_64" \
    -device virtio-blk-pci,drive=esp -monitor stdio -serial file:"$log" -display none >/dev/null 2>&1
