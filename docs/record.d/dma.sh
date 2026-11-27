# Host tests, against a fake IOMMU backend that records its mappings in an array.
cargo test -p green-tea-dma -- --test-threads=1 2>/dev/null | grep -E '^test ' | sed 's/; finished.*//' > "$OUT/test.txt"
