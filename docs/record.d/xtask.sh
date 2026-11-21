[ -f shell.gtt ] || cargo xtask trace --target riscv64 --out shell.gtt > shell.capture 2>/dev/null
cp shell.capture "$OUT/capture.txt"
head -4 shell.gtt > "$OUT/gtt-head.txt"
tail -1 shell.gtt > "$OUT/gtt-tail.txt"
cargo test -q -p xtask -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo xtask --help > "$OUT/help.txt"
cargo xtask kernel --help > "$OUT/kernel-help.txt"
cargo xtask shell --help > "$OUT/shell-help.txt"
cargo xtask trace --help > "$OUT/trace-help.txt"
cargo xtask test --help > "$OUT/test-help.txt"
cargo xtask kernel --kernel pool --target riscv64 --headless > "$OUT/kernel-pool.txt" 2>&1
cargo xtask kernel --kernel pool --kernel pool --target riscv64 --headless --jobs 1 2>&1 | grep -E '^(ok|FAIL) ' > "$OUT/kernel-twice.txt"
cargo xtask kernel --kernel nosuch > "$OUT/unknown.txt" 2>&1 || true
grep '^description' xtask/Cargo.toml > "$OUT/description.txt"
cargo metadata --no-deps --format-version 1 | python3 -I -c "
import json, sys
m = [p.split('/')[-1].split('#')[0] for p in json.load(sys.stdin)['workspace_default_members']]
print(len(m), 'default members,', len(set(m)), 'different;', 'green-tea-measure is listed', m.count('green-tea-measure'), 'times')" > "$OUT/default-members.txt"
cargo xtask kernel --board pc --kernel shell --headless > /dev/null 2>&1
cargo xtask kernel --board pc --kernel shell --headless 2>&1 | grep -v '^    Finished' > "$OUT/kernel-pc.txt"
