[ -f shell.gtt ] || cargo xtask trace --target riscv64 --out shell.gtt > shell.capture 2>/dev/null
cargo test -q -p green-tea-measure -- --test-threads=1 2>/dev/null | grep -E '^test result' | sed 's/; finished.*//' > "$OUT/test.txt"
cargo build -q --release -p green-tea-measure
M=$CARGO_TARGET_DIR/release/green-tea-measure
"$M" space shell.gtt > "$OUT/space.txt"
"$M" space shell.gtt --shuffle --only first-addr,tlsf,pools > "$OUT/space-shuffled.txt"
"$M" count shell.gtt tlsf > "$OUT/count.txt"
"$M" instructions shell.gtt > "$OUT/instructions.txt"
printf 'gtt 1\na 1000 16 16\nend 1 658740d06042d0bc\n' > one.gtt
"$M" instructions one.gtt > "$OUT/instructions-one.txt"
"$M" time shell.gtt > "$OUT/time.txt" || true
"$M" time shell.gtt --touch --only tlsf,pools > "$OUT/time-touch.txt" || true
"$M" time shell.gtt --no-warmup --only shards > "$OUT/time-no-warmup.txt" || true
failed=0
for _ in $(seq 100); do "$M" time shell.gtt --only tlsf,pools > /dev/null || failed=$((failed + 1)); done
echo "time --only tlsf,pools, run 100 times: FAIL in $failed" > "$OUT/time-flaky.txt"
"$M" runs shell.gtt tlsf first-addr > "$OUT/runs.txt"
echo "shell 20483 0000000000000000" > wrong.pins
"$M" space shell.gtt --pins wrong.pins > "$OUT/pins.txt" 2>/dev/null || true
"$M" lisp lifetimes > "$OUT/lisp-lifetimes.txt"
"$M" lisp run fib floor > "$OUT/lisp-run.txt"
"$M" lisp run fib explicit:tlsf >> "$OUT/lisp-run.txt"
"$M" lisp run fib mark-sweep 2000 >> "$OUT/lisp-run.txt"
"$M" lisp run fib counting 2000 >> "$OUT/lisp-run.txt"
"$M" lisp instructions > "$OUT/lisp-instructions.txt"
