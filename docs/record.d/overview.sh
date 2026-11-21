cargo test 2>&1 | awk '/^test result/ {p += $4; f += $6; i += $8} END {printf "host tests: %d passed, %d failed, %d ignored\n", p, f, i}' > "$OUT/tests.txt"
