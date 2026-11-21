# The trace every tool here reads: the shell's own session, captured once per work directory.
[ -f shell.gtt ] || cargo xtask trace --target riscv64 --out shell.gtt > shell.capture 2>/dev/null
cargo test -q -p green-tea-trace -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo build -q --release -p green-tea-trace
T=$CARGO_TARGET_DIR/release/green-tea-trace
"$T" stat shell.gtt > "$OUT/stat.txt"
"$T" replay shell.gtt --shuffle > "$OUT/replay.txt"
"$T" convert shell.gtt shell.gttb
"$T" stat shell.gttb > "$OUT/stat-binary.txt"
"$T" replay shell.gttb --shuffle | cmp - "$OUT/replay.txt" && echo "the replay of the binary form is byte for byte the replay of the text form" > "$OUT/same.txt"
"$T" replay shell.gtt --region-kib 1 > "$OUT/small.txt" 2>&1 || true
"$T" replay shell.gtt --region-kib 5000000 > "$OUT/large.txt" 2>&1 || true
cp shell.gtt cafe
"$T" replay --shuffle cafe > "$OUT/hex-name.txt" 2>&1 || true
rm -f cafe
# The kernel's own `replay` of the same trace is in the serial log, in the same tables.
python3 -I - "$OUT/replay.txt" > "$OUT/same-as-kernel.txt" <<'PY'
import sys
log = open("shell.gtt.log", errors="replace").read().split("\n")
host = open(sys.argv[1]).read().rstrip("\n").split("\n")
at = log.index("> replay") + 1
same = sum(a == b for a, b in zip(log[at:at + len(host)], host))
print(f"{same} of the {len(host)} lines of this replay are identical to the kernel's")
PY
