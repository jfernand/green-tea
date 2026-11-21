cargo test -q -p green-tea-acpi -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo xtask kernel --board pc --kernel smp --target x86_64 --headless >/dev/null 2>&1
cp "$CARGO_TARGET_DIR/kernel/smp-x86_64.log" "$OUT/smp-pc.txt"
# The same board on a machine with no HPET, which has to calibrate against the ACPI PM timer.
cargo xtask kernel --board pc --kernel shell --target x86_64 --headless >/dev/null 2>&1
pc=$CARGO_TARGET_DIR/kernel/pc
log=$OUT/no-hpet-pc.txt
rm -f "$log"
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$pc/nohpet.vars"
{
    for _ in $(seq 600); do grep -q '^> ' "$log" 2>/dev/null && break; sleep 0.1; done
    sleep 1
    echo quit
} | timeout 120 qemu-system-x86_64 -machine q35,hpet=off -cpu max -m 512M -vga std -net none \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$pc/nohpet.vars" \
    -drive if=none,id=esp,format=raw,readonly=on,file=fat:"$pc/shell-x86_64" \
    -device virtio-blk-pci,drive=esp -monitor stdio -serial file:"$log" -display none >/dev/null 2>&1
