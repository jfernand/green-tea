cargo test -q -p green-tea-clevo-lights -- --test-threads=1 2>/dev/null | grep -E '^test result' | head -1 | sed 's/; finished.*//' > "$OUT/test.txt"
# The pc board's shell in QEMU, whose firmware isn't a known one: the driver has to refuse.
cargo xtask kernel --board pc --kernel shell --target x86_64 --headless >/dev/null 2>&1
pc=$CARGO_TARGET_DIR/kernel/pc
log=$OUT/refused-pc.txt
rm -f "$log"
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$pc/lights.vars"
{
    for _ in $(seq 600); do grep -q '^> ' "$log" 2>/dev/null && break; sleep 0.1; done
    sleep 1
    for line in lights 'lights red' 'lights nonsense' 'lights bright 40'; do printf '%s\n' "$line"; sleep 0.7; done
    sleep 1
} | timeout 90 qemu-system-x86_64 -machine q35 -cpu max -m 512M -vga std -net none \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$pc/lights.vars" \
    -drive if=none,id=esp,format=raw,readonly=on,file=fat:"$pc/shell-x86_64" \
    -device virtio-blk-pci,drive=esp -monitor none -serial stdio -display none > "$log" 2>/dev/null || true
