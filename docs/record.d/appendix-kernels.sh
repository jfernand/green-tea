cargo xtask kernel --release --headless 2>&1 | sed -n '/^=== summary/,$p' | grep -v '^serial logs' > "$OUT/suite-release.txt"
cargo xtask kernel --release --headless --board pc --jobs 1 2>&1 | sed -n '/^=== summary/,$p' | grep -v '^serial logs' > "$OUT/suite-pc.txt"
