# green-tea-arch: the assembled switch on each architecture, and a debugger stepping through
# the first switch into a fresh task (coop-n-v2, under qemu-user for the two cross targets).
AARCH=aarch64-unknown-linux-gnu RISCV=riscv64gc-unknown-linux-gnu
cargo build -q -p green-tea-v2 --bin coop-n-v2
cargo build -q -p green-tea-v2 --bin coop-n-v2 --target $AARCH
cargo build -q -p green-tea-v2 --bin coop-n-v2 --target $RISCV
X=$CARGO_TARGET_DIR/debug/coop-n-v2
A=$CARGO_TARGET_DIR/$AARCH/debug/coop-n-v2
R=$CARGO_TARGET_DIR/$RISCV/debug/coop-n-v2
dis() { llvm-objdump -d --no-show-raw-insn --no-leading-addr ${3:-} --disassemble-symbols=swap_context,bootstrap_entry "$1" | grep -vE '^$|file format|Disassembly' > "$OUT/$2"; }
dis "$X" objdump-x86_64.txt --x86-asm-syntax=intel
dis "$A" objdump-aarch64.txt
dis "$R" objdump-riscv64.txt
G=$(mktemp -d "$CARGO_TARGET_DIR/gdb.XXXXXX")
cat > "$G/x86_64.gdb" <<'EOF'
set pagination off
set confirm off
set disassembly-flavor intel
set style enabled off
set language c
set debuginfod enabled off
break *swap_context
run > /dev/null
echo --- the context of a fresh task (rsi): rsp r15 r14 r13 r12 rbx rbp\n
x/7gx $rsi
echo --- the slot rsp points to\n
x/a *(unsigned long *)$rsi
echo --- r14\n
x/a $rsi+0x10
stepi 15
echo --- after 15 instructions\n
x/i $pc
printf "rsp mod 16 = %d\n", (unsigned long)$rsp & 15
stepi 3
echo --- after 3 more\n
x/i $pc
printf "rsp mod 16 = %d\n", (unsigned long)$rsp & 15
printf "rdi == r15: %d\n", $rdi == $r15
x/a $rsp
kill
EOF
gdb -q -batch -x "$G/x86_64.gdb" "$X" 2>&1 | sed -n '/^---/,$p' | grep -vE '^\[|^$|^Breakpoint|^warning' > "$OUT/gdb-x86_64.txt" || true

remote() { # name, qemu, sysroot, binary, port, script
    "$2" -L "$3" -g "$5" "$4" > /dev/null 2>&1 &
    local q=$!
    sleep 1
    gdb-multiarch -q -batch -ex "set sysroot $3" -ex "set debuginfod enabled off" -ex "file $4" -ex "target remote :$5" -x "$6" 2>&1 \
        | sed -n '/^---/,$p' | grep -vE '^\[|^$|^Breakpoint|^warning' > "$OUT/gdb-$1.txt" || true
    kill $q 2>/dev/null || true
    wait $q 2>/dev/null || true
}
cat > "$G/aarch64.gdb" <<'EOF'
set pagination off
set confirm off
set style enabled off
set language c
break *swap_context
continue
echo --- the context of a fresh task (x1): sp x19 x20 x21\n
x/4gx $x1
echo --- x30, at offset 96\n
x/a $x1+96
echo --- x21, at offset 24\n
x/a $x1+24
stepi 25
echo --- after 25 instructions\n
x/i $pc
printf "sp mod 16 = %d\n", (unsigned long)$sp & 15
stepi 2
echo --- after 2 more\n
x/i $pc
printf "sp mod 16 = %d\n", (unsigned long)$sp & 15
printf "x0 == x19: %d\n", $x0 == $x19
info symbol $x30
EOF
cat > "$G/riscv64.gdb" <<'EOF'
set pagination off
set confirm off
set style enabled off
set language c
break *swap_context
continue
echo --- the context of a fresh task (a1): sp ra s0 s1 s2\n
x/5gx $a1
echo --- ra, at offset 8\n
x/a $a1+8
echo --- s2, at offset 32\n
x/a $a1+32
stepi 53
echo --- after 53 instructions\n
x/i $pc
printf "sp mod 16 = %d\n", (unsigned long)$sp & 15
stepi 2
echo --- after 2 more\n
x/i $pc
printf "sp mod 16 = %d\n", (unsigned long)$sp & 15
printf "a0 == s1: %d\n", $a0 == $s1
info symbol $ra
EOF
remote aarch64 qemu-aarch64 /usr/aarch64-linux-gnu "$A" 34871 "$G/aarch64.gdb"
remote riscv64 qemu-riscv64 /usr/riscv64-linux-gnu "$R" 34872 "$G/riscv64.gdb"
rm -rf "$G"
