for k in shell trace harden region gtt resident; do kernel $k riscv64 $k-riscv64.txt; done
kernel shell aarch64 shell-aarch64.txt
# A session typed at the serial port, a line at a time. The first line is empty because
# riscv64's firmware takes the first byte (see xtask's kernel table).
typed() {
    local qemu=$1 elf=$2 file=$3; shift 3
    local machine=(-machine virt -bios default)
    [ "$qemu" = qemu-system-aarch64 ] && machine=(-machine virt,gic-version=2 -cpu cortex-a72)
    { sleep 5; printf '\n'; sleep 0.5
      for line in "$@"; do printf '%s\n' "$line"; sleep 0.5; done
      sleep 2; } |
        timeout $((10 + $# )) "$qemu" "${machine[@]}" -m 128M -device ramfb -display none \
            -serial stdio -monitor none -kernel "$elf" > "$OUT/$file" 2>/dev/null || true
}
typed qemu-system-riscv64 "$CARGO_TARGET_DIR/riscv64gc-unknown-none-elf/debug/green-tea-shell" typed-riscv64.txt \
    help 'echo hello' 'set tea green' 'set pot a small one' 'echo $tea $pot $nothing' vars 'unset pot' vars \
    mem heap map 'use heap' 'repeat 100 echo a b c' 'use arena' 'repeat 100 echo a b c' 'no such command' \
    'repeat 2 repeat 2 echo x' pages 'pages nonsense' 'trace nonsense' 'trace off' replay regions \
    'trace on' regions 'trace on' 'trace off' 'trace dump' 'repeat 1 halt'
typed qemu-system-aarch64 "$CARGO_TARGET_DIR/aarch64-unknown-none/debug/green-tea-shell" typed-aarch64.txt \
    pages halt
