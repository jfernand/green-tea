#!/usr/bin/env python3
"""Checks that every line of an #output block is in the file's recordings, docs/output/<name>/.
Usage: python3 -I docs/tools/check-quotes.py docs/crates/fit.typ ..."""
import glob, os, re, sys
norm = lambda l: re.sub(r"\s+", " ", l).strip()
root, bad = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "output"), 0
for file in sys.argv[1:]:
    name = os.path.splitext(os.path.basename(file))[0]
    recorded = set()
    for path in glob.glob(os.path.join(root, name, "*")):
        with open(path, errors="replace") as f: recorded.update(norm(l) for l in f)
    blocks = re.findall(r'#output\(title: "((?:[^"\\]|\\.)*)"[^\n]*\n?\s*```\w*\n(.*?)\n\s*```', open(file).read(), re.S)
    n = 0
    for title, body in blocks:
        for line in body.split("\n"):
            q = norm(line)
            if not q or q == "…": continue
            n += 1
            if q not in recorded and not any(q in r for r in recorded):
                bad += 1; print(f"{file}: [{title}] not recorded: {line}")
    print(f"{file}: {len(blocks)} output blocks, {n} lines checked")
sys.exit(1 if bad else 0)
