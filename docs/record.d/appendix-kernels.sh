cargo xtask kernel --release --headless 2>&1 | sed -n '/^=== summary/,$p' | grep -v '^serial logs' > "$OUT/suite-release.txt"
