#!/usr/bin/env python3
"""Checks every #srcfile listing against the pinned commit, line by line, ignoring whitespace.
Usage: python3 -I docs/tools/check-snippets.py docs/crates/fit.typ ..."""
import re, subprocess, sys
PIN = "59e8c91"
norm = lambda t: re.sub(r"\s+", "", t)
cache, bad = {}, 0
def source(path):
    if path not in cache:
        cache[path] = subprocess.run(["git", "show", f"{PIN}:{path}"], capture_output=True, text=True, check=True).stdout.split("\n")
    return cache[path]
for file in sys.argv[1:]:
    text, count = open(file).read(), 0
    for m in re.finditer(r'#srcfile\("([^"]+)",\s*lines:\s*\(((?:\s*\(\d+,\s*\d+\),?)+)\s*\)(.*?)\[```\w*\n(.*?)\n```\]', text, re.S):
        path, ranges, code = m.group(1), m.group(2), m.group(4)
        nums = [n for a, b in re.findall(r"\((\d+),\s*(\d+)\)", ranges) for n in range(int(a), int(b) + 1)]
        lines = []
        for l in code.split("\n"):
            if l.strip() == "// …": continue
            if l.startswith("»"): lines[-1] += l[1:]; continue
            lines.append(l)
        orig = source(path)
        if len(lines) != len(nums):
            bad += 1; print(f"{file}: {path}: {len(lines)} lines for {len(nums)} line numbers")
        for n, l in zip(nums, lines):
            if n > len(orig) or norm(l) != norm(orig[n - 1]):
                bad += 1; print(f"{file}: MISMATCH {path}:{n}\n  report: {l.strip()}\n  source: {orig[n-1].strip() if n <= len(orig) else '(past the end)'}")
        count += 1
    if text.count("#srcfile(") != count:
        bad += 1; print(f"{file}: {text.count('#srcfile(')} listings, parsed {count}")
    print(f"{file}: {count} listings checked")
sys.exit(1 if bad else 0)
