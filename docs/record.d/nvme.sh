cargo test -q -p green-tea-nvme -- --test-threads=1 2>/dev/null | grep -E '^test result' | head -1 | sed 's/; finished.*//' > "$OUT/test.txt"
cargo xtask kernel --board pc --kernel disk --target x86_64 --headless >/dev/null 2>&1
cp "$CARGO_TARGET_DIR/kernel/disk-x86_64.log" "$OUT/disk-pc.txt"
# The same image with a disk whose serial isn't the test disk's, and whose bytes must not change.
pc=$CARGO_TARGET_DIR/kernel/pc
log=$OUT/refused-pc.txt
rm -f "$log"
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$pc/refused.vars"
head -c 32768 /dev/urandom > "$pc/other.img"
before=$(sha256sum < "$pc/other.img")
timeout 40 qemu-system-x86_64 -machine q35 -cpu max -m 512M -vga std -net none \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$pc/refused.vars" \
    -drive if=none,id=esp,format=raw,readonly=on,file=fat:"$pc/disk-x86_64" \
    -device virtio-blk-pci,drive=esp \
    -drive if=none,id=disk,format=raw,file="$pc/other.img" -device nvme,drive=disk,serial=somebodys-ssd \
    -serial file:"$log" -monitor none -display none >/dev/null 2>&1 || true
[ "$(sha256sum < "$pc/other.img")" = "$before" ] && echo "the disk's 32 KiB are unchanged" >> "$log"
