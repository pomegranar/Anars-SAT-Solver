#!/usr/bin/env python3
"""Generate CNF instances that probe where component caching helps and where it does not.

The SATLIB corpus is mostly random 3-SAT, which is the *worst* case for a decomposing solver:
the constraint graph of a random formula at the phase transition is an expander, so it never
splits and the memo is pure overhead. To say anything honest about the design, the benchmark set
also needs instances with structure.

Families
--------
rand3     random 3-SAT at ratio 4.26            no structure; the memo should not help
php       pigeonhole, n+1 pigeons into n holes  exponential for resolution; highly connected
blocks    k independent random 3-SAT blocks     best case: the formula is already decomposed
chain     k blocks joined in a path             low treewidth; the realistic good case
grid      3-colouring of an m x n grid graph    low treewidth, and a real problem shape

Usage: gen_instances.py <output-dir>
"""

from __future__ import annotations

import pathlib
import random
import sys


def write_cnf(path: pathlib.Path, num_vars: int, clauses: list[list[int]], comment: str) -> None:
    lines = [f"c {comment}", f"p cnf {num_vars} {len(clauses)}"]
    lines += [" ".join(map(str, c)) + " 0" for c in clauses]
    path.write_text("\n".join(lines) + "\n")


def random_3sat(rng: random.Random, num_vars: int, num_clauses: int, offset: int = 0) -> list[list[int]]:
    """Random 3-SAT over variables `offset+1 ..= offset+num_vars`."""
    clauses = []
    for _ in range(num_clauses):
        picked = rng.sample(range(1, num_vars + 1), 3)
        clauses.append([rng.choice((1, -1)) * (v + offset) for v in picked])
    return clauses


def pigeonhole(holes: int) -> tuple[int, list[list[int]]]:
    """n+1 pigeons into n holes: unsatisfiable, and famously exponential for resolution."""
    pigeons = holes + 1

    def var(p: int, h: int) -> int:
        return p * holes + h + 1

    clauses = [[var(p, h) for h in range(holes)] for p in range(pigeons)]
    for h in range(holes):
        for p in range(pigeons):
            for q in range(p + 1, pigeons):
                clauses.append([-var(p, h), -var(q, h)])
    return pigeons * holes, clauses


def blocks(rng: random.Random, count: int, vars_per_block: int, ratio: float) -> tuple[int, list[list[int]]]:
    """Independent random 3-SAT blocks sharing no variables at all."""
    clauses: list[list[int]] = []
    for b in range(count):
        clauses += random_3sat(rng, vars_per_block, int(vars_per_block * ratio), b * vars_per_block)
    return count * vars_per_block, clauses


def chain(rng: random.Random, count: int, vars_per_block: int, ratio: float) -> tuple[int, list[list[int]]]:
    """Blocks joined in a path, each sharing an implication with the next.

    Treewidth stays tiny however long the chain gets, so branching on the linking variables
    should split the formula into pieces the memo can reuse.
    """
    clauses: list[list[int]] = []
    for b in range(count):
        offset = b * vars_per_block
        clauses += random_3sat(rng, vars_per_block, int(vars_per_block * ratio), offset)
        if b + 1 < count:
            # Tie this block's last variable to the next block's first, in both directions.
            here = offset + vars_per_block
            nxt = offset + vars_per_block + 1
            clauses.append([-here, nxt])
            clauses.append([here, -nxt])
    return count * vars_per_block, clauses


def grid_colouring(width: int, height: int, colours: int = 3) -> tuple[int, list[list[int]]]:
    """3-colour an m x n grid graph. Always satisfiable, and of treewidth min(m, n)."""

    def var(x: int, y: int, c: int) -> int:
        return (y * width + x) * colours + c + 1

    clauses = []
    for y in range(height):
        for x in range(width):
            clauses.append([var(x, y, c) for c in range(colours)])
            for c in range(colours):
                for d in range(c + 1, colours):
                    clauses.append([-var(x, y, c), -var(x, y, d)])
    for y in range(height):
        for x in range(width):
            for dx, dy in ((1, 0), (0, 1)):
                nx, ny = x + dx, y + dy
                if nx < width and ny < height:
                    for c in range(colours):
                        clauses.append([-var(x, y, c), -var(nx, ny, c)])
    return width * height * colours, clauses


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    out = pathlib.Path(argv[1])
    made = 0

    d = out / "php"
    d.mkdir(parents=True, exist_ok=True)
    for holes in range(6, 13):
        n, clauses = pigeonhole(holes)
        write_cnf(d / f"php-{holes + 1}-{holes}.cnf", n, clauses,
                  f"pigeonhole: {holes + 1} pigeons into {holes} holes (UNSAT)")
        made += 1

    d = out / "rand3"
    d.mkdir(parents=True, exist_ok=True)
    for num_vars in (150, 175, 200, 225, 250, 275, 300):
        for seed in range(5):
            rng = random.Random(1000 * num_vars + seed)
            clauses = random_3sat(rng, num_vars, int(num_vars * 4.26))
            write_cnf(d / f"rand3-{num_vars}-{seed}.cnf", num_vars, clauses,
                      f"random 3-SAT, {num_vars} vars at ratio 4.26")
            made += 1

    d = out / "blocks"
    d.mkdir(parents=True, exist_ok=True)
    for count in (4, 8, 16, 32):
        for seed in range(3):
            rng = random.Random(7000 + count * 10 + seed)
            n, clauses = blocks(rng, count, 40, 4.26)
            write_cnf(d / f"blocks-{count}x40-{seed}.cnf", n, clauses,
                      f"{count} independent random 3-SAT blocks of 40 variables")
            made += 1

    d = out / "chain"
    d.mkdir(parents=True, exist_ok=True)
    for count in (4, 8, 16, 32, 64):
        for seed in range(3):
            rng = random.Random(9000 + count * 10 + seed)
            n, clauses = chain(rng, count, 30, 4.0)
            write_cnf(d / f"chain-{count}x30-{seed}.cnf", n, clauses,
                      f"{count} random 3-SAT blocks joined in a path")
            made += 1

    d = out / "grid"
    d.mkdir(parents=True, exist_ok=True)
    for width, height in ((6, 6), (8, 8), (10, 10), (6, 20), (5, 40), (4, 80)):
        n, clauses = grid_colouring(width, height)
        write_cnf(d / f"grid-{width}x{height}.cnf", n, clauses,
                  f"3-colouring of a {width}x{height} grid graph (SAT)")
        made += 1

    print(f"generated {made} instances under {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
