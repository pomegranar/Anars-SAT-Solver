#!/usr/bin/env python3
"""Benchmark dpbst against real SAT solvers, and against itself.

Runs a set of solvers over a set of SATLIB suites, records wall time, verdict and peak memory
per instance, cross-checks every verdict for disagreement, and prints a summary table.

Two things it deliberately does:

* Runs sequentially by default (`--jobs 1`). Timing several solvers at once on the same machine
  is how benchmark tables become fiction.
* Treats a disagreement as a hard error. A solver that is fast and wrong is worth nothing, so
  any conflicting verdict is reported loudly rather than averaged away.

Examples
--------
    # headline comparison
    scripts/bench.py --suites uf50-218:100 uuf50-218:100 --timeout 10

    # dpbst internals only: which bucket policy wins?
    scripts/bench.py --solvers dpbst-avl dpbst-unbalanced dpbst-splay dpbst-chain \\
        --suites flat50-115:100 --timeout 10

    # correctness sweep, parallel because only verdicts matter
    scripts/bench.py --mode validate --jobs 8 --suites uf100-430:300 --timeout 20
"""

from __future__ import annotations

import argparse
import concurrent.futures
import csv
import json
import os
import pathlib
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
DPBST = ROOT / "target" / "release" / "dpbst"
TOOLS = ROOT / "tools" / "bin"
CLEAN = ROOT / "benchmarks" / "clean"

# Exit codes are the SAT competition convention, which every solver here follows.
CODE_TO_STATUS = {10: "SAT", 20: "UNSAT"}


def dpbst_variant(*flags: str) -> list[str]:
    return [str(DPBST), "--no-model", *flags]


# name -> argv prefix; the instance path is appended.
SOLVERS: dict[str, list[str]] = {
    # dpbst configurations
    "dpbst": dpbst_variant(),
    "dpbst-avl": dpbst_variant("--bucket", "avl"),
    "dpbst-unbalanced": dpbst_variant("--bucket", "unbalanced"),
    "dpbst-splay": dpbst_variant("--bucket", "splay"),
    "dpbst-chain": dpbst_variant("--bucket", "chain"),
    "dpbst-nocache": dpbst_variant("--no-cache"),
    "dpbst-plain-dpll": dpbst_variant("--algorithm", "dpll", "--no-cache", "--no-preprocess"),
    "dpbst-davis-putnam": dpbst_variant("--algorithm", "dp"),
    "dpbst-nopure": dpbst_variant("--no-pure-literals"),
    "dpbst-nopre": dpbst_variant("--no-preprocess"),
    "dpbst-jw": dpbst_variant("--heuristic", "jw"),
    "dpbst-dlis": dpbst_variant("--heuristic", "dlis"),
    "dpbst-dlcs": dpbst_variant("--heuristic", "dlcs"),
    "dpbst-mom": dpbst_variant("--heuristic", "mom"),
    "dpbst-static": dpbst_variant("--heuristic", "static"),
    # reference solvers, built from source under tools/
    "kissat": [str(TOOLS / "kissat"), "-q"],
    "cadical": [str(TOOLS / "cadical"), "-q"],
    "minisat": [str(TOOLS / "minisat"), "-verb=0"],
    "varisat": [str(TOOLS / "varisat")],
    "splr": [str(TOOLS / "splr"), "-q"],
}

DEFAULT_SOLVERS = ["dpbst", "minisat", "varisat", "splr", "cadical", "kissat"]


class Result:
    __slots__ = ("solver", "instance", "suite", "status", "seconds", "peak_kb", "timed_out")

    def __init__(self, solver, instance, suite, status, seconds, peak_kb, timed_out):
        self.solver = solver
        self.instance = instance
        self.suite = suite
        self.status = status
        self.seconds = seconds
        self.peak_kb = peak_kb
        self.timed_out = timed_out


def run_one(
    solver: str,
    argv: list[str],
    path: pathlib.Path,
    suite: str,
    timeout: float,
    sandbox: str,
) -> Result:
    """Runs one solver on one instance, measuring wall time and peak resident memory.

    Runs in `sandbox` rather than the repository: splr writes an `ans_<instance>.cnf` answer
    file into its working directory for every run, and 800 of those in the project root is not
    a benchmark artefact anyone asked for.
    """
    command = [*argv, str(path.resolve())]
    started = time.perf_counter()
    peak_kb = 0
    try:
        process = subprocess.Popen(
            command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=sandbox
        )
    except OSError as exc:
        return Result(solver, path.name, suite, f"ERROR:{exc}", 0.0, 0, False)

    timed_out = False
    try:
        # os.wait4 gives rusage, which subprocess.run does not expose.
        deadline = started + timeout
        while True:
            pid, raw_status, usage = os.wait4(process.pid, os.WNOHANG)
            if pid:
                code = os.waitstatus_to_exitcode(raw_status) if raw_status >= 0 else -1
                # macOS reports ru_maxrss in bytes, Linux in kibibytes.
                peak_kb = usage.ru_maxrss // 1024 if sys.platform == "darwin" else usage.ru_maxrss
                break
            if time.perf_counter() > deadline:
                process.kill()
                os.wait4(process.pid, 0)
                timed_out = True
                code = None
                break
            time.sleep(0.0005)
    except ChildProcessError:
        code = process.wait()

    seconds = time.perf_counter() - started
    if timed_out:
        status = "TIMEOUT"
    elif code in CODE_TO_STATUS:
        status = CODE_TO_STATUS[code]
    elif code == 0:
        status = "UNKNOWN"
    else:
        status = f"ERROR:{code}"
    return Result(solver, path.name, suite, status, seconds, peak_kb, timed_out)


def collect_instances(specs: list[str]) -> list[tuple[str, pathlib.Path]]:
    """Expands `suite:count` specifications into concrete instance paths."""
    instances: list[tuple[str, pathlib.Path]] = []
    for spec in specs:
        name, _, count = spec.partition(":")
        directory = CLEAN / name
        if not directory.is_dir():
            sys.exit(f"no such suite: {directory} (run scripts/fetch_benchmarks.sh first)")
        files = sorted(directory.glob("*.cnf"))
        if count:
            files = files[: int(count)]
        instances.extend((name, f) for f in files)
    return instances


def summarize(results: list[Result], solvers: list[str], timeout: float) -> str:
    """Builds a markdown table: solved counts and timing per solver per suite."""
    suites = sorted({r.suite for r in results})
    lines = []
    for suite in suites:
        total = len({r.instance for r in results if r.suite == suite})
        lines.append(f"\n### {suite} ({total} instances, {timeout:g}s timeout)\n")
        lines.append("| solver | solved | timeouts | total s | mean s | median s | max s | peak MiB |")
        lines.append("|---|---:|---:|---:|---:|---:|---:|---:|")
        rows = []
        for solver in solvers:
            subset = [r for r in results if r.suite == suite and r.solver == solver]
            if not subset:
                continue
            solved = [r for r in subset if r.status in ("SAT", "UNSAT")]
            timeouts = sum(1 for r in subset if r.timed_out)
            times = [r.seconds for r in solved] or [0.0]
            peak = max((r.peak_kb for r in subset), default=0) / 1024
            rows.append(
                (
                    sum(r.seconds for r in subset),
                    f"| `{solver}` | {len(solved)}/{len(subset)} | {timeouts} | "
                    f"{sum(r.seconds for r in subset):.2f} | {statistics.mean(times):.4f} | "
                    f"{statistics.median(times):.4f} | {max(times):.4f} | {peak:.1f} |",
                )
            )
        for _, row in sorted(rows):
            lines.append(row)
    return "\n".join(lines)


def check_agreement(results: list[Result]) -> list[str]:
    """Reports any instance where two solvers returned conflicting verdicts."""
    verdicts: dict[tuple[str, str], dict[str, str]] = {}
    for r in results:
        if r.status in ("SAT", "UNSAT"):
            verdicts.setdefault((r.suite, r.instance), {})[r.solver] = r.status

    problems = []
    for (suite, instance), by_solver in sorted(verdicts.items()):
        distinct = set(by_solver.values())
        if len(distinct) > 1:
            detail = ", ".join(f"{s}={v}" for s, v in sorted(by_solver.items()))
            problems.append(f"{suite}/{instance}: {detail}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--solvers", nargs="+", default=DEFAULT_SOLVERS,
                        help=f"solvers to run; known: {', '.join(SOLVERS)}")
    parser.add_argument("--suites", nargs="+", default=["uf50-218:50", "uuf50-218:50"],
                        help="suite specifications as NAME or NAME:COUNT")
    parser.add_argument("--timeout", type=float, default=10.0, help="per-instance seconds")
    parser.add_argument("--jobs", type=int, default=1,
                        help="parallel runs; keep at 1 for trustworthy timings")
    parser.add_argument("--mode", choices=["time", "validate"], default="time")
    parser.add_argument("--out", type=pathlib.Path, default=ROOT / "results" / "bench.csv")
    parser.add_argument("--markdown", type=pathlib.Path, default=None,
                        help="also write the summary table here")
    args = parser.parse_args()

    unknown = [s for s in args.solvers if s not in SOLVERS]
    if unknown:
        sys.exit(f"unknown solver(s): {', '.join(unknown)}")
    missing = [s for s in args.solvers if not shutil.which(SOLVERS[s][0])]
    if missing:
        sys.exit(f"binary not found for: {', '.join(missing)}. Build it first.")

    instances = collect_instances(args.suites)
    jobs = [
        (solver, SOLVERS[solver], path, suite)
        for suite, path in instances
        for solver in args.solvers
    ]
    print(
        f"{len(instances)} instances x {len(args.solvers)} solvers = {len(jobs)} runs "
        f"({args.timeout:g}s timeout, {args.jobs} job(s))",
        file=sys.stderr,
    )

    results: list[Result] = []
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix="dpbst-bench-") as sandbox:
        if args.jobs > 1:
            with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
                futures = [
                    pool.submit(run_one, s, a, p, su, args.timeout, sandbox)
                    for s, a, p, su in jobs
                ]
                for i, future in enumerate(concurrent.futures.as_completed(futures), 1):
                    results.append(future.result())
                    if i % 50 == 0:
                        print(f"  {i}/{len(jobs)}", file=sys.stderr)
        else:
            for i, (solver, argv, path, suite) in enumerate(jobs, 1):
                results.append(run_one(solver, argv, path, suite, args.timeout, sandbox))
                if i % 50 == 0:
                    print(f"  {i}/{len(jobs)}", file=sys.stderr)
    print(f"done in {time.perf_counter() - started:.1f}s", file=sys.stderr)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(["solver", "suite", "instance", "status", "seconds", "peak_kb"])
        for r in sorted(results, key=lambda r: (r.suite, r.instance, r.solver)):
            writer.writerow([r.solver, r.suite, r.instance, r.status, f"{r.seconds:.6f}", r.peak_kb])
    print(f"wrote {args.out}", file=sys.stderr)

    problems = check_agreement(results)
    if problems:
        print("\n!! VERDICT DISAGREEMENTS !!", file=sys.stderr)
        for p in problems:
            print("  " + p, file=sys.stderr)

    if args.mode == "time":
        table = summarize(results, args.solvers, args.timeout)
        print(table)
        if args.markdown:
            args.markdown.parent.mkdir(parents=True, exist_ok=True)
            args.markdown.write_text(table + "\n")
    else:
        solved = sum(1 for r in results if r.status in ("SAT", "UNSAT"))
        print(f"{solved}/{len(results)} runs produced a verdict; "
              f"{len(problems)} disagreement(s)")

    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
