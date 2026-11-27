# A failing kernel is part of the record, so a failed suite doesn't stop the recording.
{ cargo xtask kernel --release --headless 2>&1 || true; } | sed -n '/^=== summary/,$p' | grep -v '^serial logs' > "$OUT/suite-release.txt"
{ cargo xtask kernel --release --headless --board pc --jobs 1 2>&1 || true; } | sed -n '/^=== summary/,$p' | grep -v '^serial logs' > "$OUT/suite-pc.txt"
