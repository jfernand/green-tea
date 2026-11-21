#!/usr/bin/env python3
"""Expands listing markers in a .typ file, in place, from the pinned commit. A marker is one line:
    %%SRC path/to/file.rs 10-20 31-40%%
    %%SRC path/to/file.rs 10-20 | notes: ((12, [What line 12 does.]),)%%
It becomes a #srcfile call with those lines, dedented, and `// …` between ranges.
Usage: python3 -I docs/tools/expand.py docs/crates/fit.typ ..."""
import re, subprocess, sys
PIN = "79b31d6"
LANG = {"rs": "rust", "lisp": "lisp", "ld": "text", "toml": "toml", "yml": "yaml", "S": "text"}
def expand(m):
    spec, _, extra = m.group(1).partition("|")
    path, *ranges = spec.split()
    src = subprocess.run(["git", "show", f"{PIN}:{path}"], capture_output=True, text=True, check=True).stdout.split("\n")
    pairs = [tuple(map(int, (r.split("-") * 2)[:2])) for r in ranges]
    chosen = [src[n - 1] for a, b in pairs for n in range(a, b + 1)]
    indent = min((len(l) - len(l.lstrip()) for l in chosen if l.strip()), default=0)
    body = []
    for i, (a, b) in enumerate(pairs):
        if i: body.append("// …")
        body += [src[n - 1][indent:] for n in range(a, b + 1)]
    lines = ", ".join(f"({a}, {b})" for a, b in pairs)
    args = f"lines: ({lines},)" + (", " + extra.strip() if extra.strip() else "")
    return f'#srcfile("{path}", {args})[```{LANG.get(path.rsplit(".", 1)[-1], "text")}\n' + "\n".join(body) + "\n```]"
for f in sys.argv[1:]:
    t = open(f).read()
    new = re.sub(r"^%%SRC (.+?)%%$", expand, t, flags=re.M)
    if new != t: open(f, "w").write(new)
