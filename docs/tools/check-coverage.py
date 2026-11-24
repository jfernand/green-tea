#!/usr/bin/env python3
"""Lists what the pinned commit has and the report doesn't name: crates, modules, public items,
kernels, Lisp programs, xtask commands. Usage: python3 -I docs/tools/check-coverage.py [crate ...]"""
import glob, os, re, subprocess, sys
PIN = "aecf66f"
docs = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
git = lambda *a: subprocess.run(["git", *a], capture_output=True, text=True, check=True, cwd=docs).stdout
files = git("ls-tree", "-r", "--name-only", PIN, "--full-tree").split("\n")
dirs = sorted({f.split("/Cargo.toml")[0] for f in files if f.endswith("/Cargo.toml")})
short = lambda d: {"xtask": "xtask"}.get(d, d.split("/")[-1].removeprefix("green-tea-"))
only, missing, total = set(sys.argv[1:]), 0, 0
ITEM = re.compile(r"^\s*pub\s+(?:unsafe\s+|const\s+(?=fn)|extern\s+\"C\"\s+)*(?:fn|struct|enum|trait|const|static|type|mod|union)\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)", re.M)
for d in dirs:
    c = short(d)
    if only and c not in only: continue
    path = os.path.join(docs, "crates", c + ".typ")
    text = open(path).read() if os.path.exists(path) else ""
    gaps = []
    for f in [f for f in files if f.startswith(d + "/") and f.endswith((".rs", ".lisp"))]:
        stem = os.path.splitext(os.path.basename(f))[0]
        if stem not in ("lib", "main", "mod", "build") and not re.search(r"\b" + re.escape(stem) + r"\b", text): gaps.append("module " + stem)
        total += 1
        if f.endswith(".rs") and "/tests" not in f and not f.endswith("_tests.rs"):
            for name in sorted(set(ITEM.findall(git("show", f"{PIN}:{f}").split("#[cfg(test)]\nmod ")[0]))):
                total += 1
                if not re.search(r"\b" + re.escape(name) + r"\b", text): gaps.append(f"{os.path.basename(f)}: {name}")
    if gaps:
        missing += len(gaps); print(f"{c}: {len(gaps)} not named\n  " + "\n  ".join(gaps))
if not only:
    all_text = "".join(open(p).read() for p in glob.glob(os.path.join(docs, "**", "*.typ"), recursive=True) if not p.endswith(("isss-template.typ", "lib.typ")))
    kernels = re.findall(r'Kernel \{\s*name: "([a-z0-9]+)"', git("show", f"{PIN}:xtask/src/kernel.rs"))
    for k in kernels:
        total += 1
        if not re.search(r"`" + k + r"`", all_text): missing += 1; print("kernel not named:", k)
print(f"{total} checked, {missing} not named")
sys.exit(1 if missing else 0)
