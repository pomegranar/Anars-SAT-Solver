#!/usr/bin/env python3
"""Strip SATLIB's trailing `%` marker from DIMACS files.

Most of the SATLIB corpus ends with

    ...
    -7 -2 6 0
    %
    0

The `%` is an end-of-instance marker, and the `0` after it is not a clause. `dpbst` handles
this, but MiniSat, CaDiCaL and Kissat all refuse to parse such a file, so a fair comparison
needs a normalised copy of the corpus. Reading past the `%` would invent an empty clause and
turn every instance unsatisfiable, so this is a correctness issue, not a cosmetic one.

Usage: normalize_cnf.py <src-dir> <dst-dir>
"""

from __future__ import annotations

import pathlib
import sys


def normalize(text: str) -> str:
    out = []
    for line in text.splitlines():
        if line.strip().startswith("%"):
            break
        out.append(line)
    return "\n".join(out).rstrip() + "\n"


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2

    src, dst = pathlib.Path(argv[1]), pathlib.Path(argv[2])
    changed = total = 0
    for path in sorted(src.rglob("*.cnf")):
        target = dst / path.relative_to(src)
        target.parent.mkdir(parents=True, exist_ok=True)
        original = path.read_text(errors="replace")
        cleaned = normalize(original)
        target.write_text(cleaned)
        total += 1
        changed += cleaned != original

    print(f"normalised {total} files, {changed} had a '%' marker -> {dst}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
