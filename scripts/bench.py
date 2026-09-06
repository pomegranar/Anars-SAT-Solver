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
import collections
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

def performance_cores() -> int:
    """Number of performance cores, so parallel runs do not spill onto the slow ones.

    Apple silicon is heterogeneous: an M1 has four performance and four efficiency cores, and
    macOS offers userspace no affinity control. Oversubscribing past the performance cores means
    some runs land on efficiency cores at roughly a third of the speed, which biases whichever
    solver happens to be scheduled there rather than adding symmetric noise.
    """
    try:
        out = subprocess.run(
            ["sysctl", "-n", "hw.perflevel0.logicalcpu"],
            capture_output=True, text=True, check=True,
        )
        return max(1, int(out.stdout.strip()))
    except (OSError, ValueError, subprocess.SubprocessError):
        return max(1, (os.cpu_count() or 2) // 2)


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


# Deterministic counters reported by `dpbst --json`. These do not depend on how loaded the
# machine is, which is what makes `--mode metrics` safe to run in parallel.
METRIC_FIELDS = (
    "nodes",
    "decisions",
    "conflicts",
    "propagations",
    "components",
    "cache_lookups",
    "cache_hits",
    "cache_hit_rate",
    "cache_entries",
    "table_max_bucket",
    "table_max_depth",
    "table_mean_depth",
)


class Result:
    __slots__ = (
        "solver", "instance", "suite", "status", "seconds", "peak_kb", "timed_out", "metrics",
    )

    def __init__(self, solver, instance, suite, status, seconds, peak_kb, timed_out, metrics=None):
        self.solver = solver
        self.instance = instance
        self.suite = suite
        self.status = status
        self.seconds = seconds
        self.peak_kb = peak_kb
        self.timed_out = timed_out
        self.metrics = metrics or {}


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

    For dpbst configurations the solver's own `--json` statistics are captured from stderr, so a
    run yields deterministic counters as well as a time.
    """
    wants_json = solver.startswith("dpbst")
    command = [*argv, *(["--json"] if wants_json else []), str(path.resolve())]
    started = time.perf_counter()
    peak_kb = 0
    stats_sink = subprocess.PIPE if wants_json else subprocess.DEVNULL
    try:
        process = subprocess.Popen(
            command, stdout=subprocess.DEVNULL, stderr=stats_sink, cwd=sandbox
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

    metrics: dict[str, str] = {}
    if wants_json and process.stderr is not None:
        try:
            raw = process.stderr.read().decode("utf-8", "replace")
        except (OSError, ValueError):
            raw = ""
        finally:
            process.stderr.close()
        # The JSON object is the last line; warnings may precede it.
        for line in reversed(raw.strip().splitlines()):
            if line.startswith("{"):
                try:
                    metrics = json.loads(line)
                except json.JSONDecodeError:
                    metrics = {}
                break

    if timed_out:
        status = "TIMEOUT"
    elif code in CODE_TO_STATUS:
        status = CODE_TO_STATUS[code]
    elif code == 0:
        status = "UNKNOWN"
    else:
        status = f"ERROR:{code}"
    return Result(solver, path.name, suite, status, seconds, peak_kb, timed_out, metrics)


def header_matches_body(path: pathlib.Path) -> bool:
    """Whether a DIMACS file's `p cnf` header agrees with the clauses it actually contains.

    Ten files in the SATLIB corpus fail this, and each one silently changes the question being
    asked. `dubois100.cnf` is missing two `0` terminators, so a strict parser merges four
    clauses into two tautologies and the unsatisfiable instance becomes satisfiable. Every
    `pret*.cnf` ends with a stray `0`, which is a legal *empty clause* and makes the instance
    unsatisfiable by inspection. Benchmarking on those measures parser leniency, not solvers, so
    they are skipped.
    """
    declared = None
    clauses = dangling = 0
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return False
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped[0] == "c":
            continue
        if stripped[0] == "p":
            parts = stripped.split()
            if len(parts) >= 4:
                declared = int(parts[3])
            continue
        if stripped[0] == "%":
            break
        for token in stripped.split():
            try:
                value = int(token)
            except ValueError:
                continue
            if value == 0:
                clauses += 1
                dangling = 0
            else:
                dangling += 1
    return declared is None or (clauses == declared and dangling == 0)


def collect_instances(specs: list[str]) -> list[tuple[str, pathlib.Path]]:
    """Expands `suite:count` specifications into concrete instance paths."""
    instances: list[tuple[str, pathlib.Path]] = []
    skipped = 0
    for spec in specs:
        name, _, count = spec.partition(":")
        directory = CLEAN / name
        if not directory.is_dir():
            sys.exit(f"no such suite: {directory} (run scripts/fetch_benchmarks.sh first)")
        files = sorted(directory.glob("*.cnf"))
        if count:
            files = files[: int(count)]
        for f in files:
            if header_matches_body(f):
                instances.append((name, f))
            else:
                skipped += 1
                print(f"skipping malformed instance: {name}/{f.name}", file=sys.stderr)
    if skipped:
        print(f"skipped {skipped} malformed instance(s)", file=sys.stderr)
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


def summarize_metrics(results: list[Result], solvers: list[str]) -> str:
    """Builds a markdown table of the solver's own deterministic counters.

    None of these depend on machine load, so unlike `summarize` this is meaningful even when the
    runs were executed in parallel.
    """
    suites = sorted({r.suite for r in results})
    lines = []
    for suite in suites:
        lines.append(f"\n### {suite}\n")
        lines.append(
            "| config | solved | search nodes | components | memo hits | hit rate | "
            "entries | max bucket | mean depth |"
        )
        lines.append("|---|---:|---:|---:|---:|---:|---:|---:|---:|")
        for solver in solvers:
            subset = [
                r for r in results
                if r.suite == suite and r.solver == solver and r.metrics
            ]
            if not subset:
                continue
            solved = [r for r in subset if r.status in ("SAT", "UNSAT")]

            def total(field: str) -> float:
                return sum(float(r.metrics.get(field, 0) or 0) for r in subset)

            lookups = total("cache_lookups")
            hits = total("cache_hits")
            rate = hits / lookups if lookups else 0.0
            depths = [
                float(r.metrics["table_mean_depth"])
                for r in subset
                if float(r.metrics.get("table_mean_depth", 0) or 0) > 0
            ]
            buckets = [int(r.metrics.get("table_max_bucket", 0) or 0) for r in subset]
            lines.append(
                f"| `{solver}` | {len(solved)}/{len(subset)} | {total('nodes'):.0f} | "
                f"{total('components'):.0f} | {hits:.0f} | {rate * 100:.1f}% | "
                f"{total('cache_entries'):.0f} | {max(buckets, default=0)} | "
                f"{statistics.mean(depths) if depths else 0.0:.2f} |"
            )
    return "\n".join(lines)


def check_agreement(results: list[Result]) -> tuple[list[str], list[str]]:
    """Finds instances where solvers returned conflicting verdicts.

    Returns `(disagreements, ours)`. Every conflict is reported, but only those where a `dpbst`
    configuration sided against the majority count as a failure of *this* project: the reference
    solvers disagreeing with each other is a fact about them. splr 0.19, for example, reports
    `par16-*` unsatisfiable where CaDiCaL, Kissat, MiniSat, varisat and dpbst all produce a
    verified model.
    """
    verdicts: dict[tuple[str, str], dict[str, str]] = {}
    for r in results:
        if r.status in ("SAT", "UNSAT"):
            verdicts.setdefault((r.suite, r.instance), {})[r.solver] = r.status

    disagreements: list[str] = []
    ours: list[str] = []
    for (suite, instance), by_solver in sorted(verdicts.items()):
        if len(set(by_solver.values())) <= 1:
            continue
        detail = ", ".join(f"{s}={v}" for s, v in sorted(by_solver.items()))
        line = f"{suite}/{instance}: {detail}"
        disagreements.append(line)

        tally = collections.Counter(by_solver.values())
        majority, _ = tally.most_common(1)[0]
        if any(s.startswith("dpbst") and v != majority for s, v in by_solver.items()):
            ours.append(line)
    return disagreements, ours


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--solvers", nargs="+", default=DEFAULT_SOLVERS,
                        help=f"solvers to run; known: {', '.join(SOLVERS)}")
    parser.add_argument("--suites", nargs="+", default=["uf50-218:50", "uuf50-218:50"],
                        help="suite specifications as NAME or NAME:COUNT")
    parser.add_argument("--timeout", type=float, default=10.0, help="per-instance seconds")
    parser.add_argument(
        "--jobs", type=int, default=0,
        help="parallel runs. Defaults to 1 for --mode time (concurrent runs contend for cache "
             "and memory bandwidth, and on Apple silicon the surplus lands on the slower "
             "efficiency cores) and to the performance-core count otherwise, where only "
             "load-independent counters are reported.",
    )
    parser.add_argument(
        "--mode", choices=["time", "metrics", "validate"], default="time",
        help="time: wall-clock comparison, run sequentially. metrics: the solver's own "
             "deterministic counters, safe to run in parallel. validate: verdicts only.",
    )
    parser.add_argument("--out", type=pathlib.Path, default=ROOT / "results" / "bench.csv")
    parser.add_argument("--markdown", type=pathlib.Path, default=None,
                        help="also write the summary table here")
    args = parser.parse_args()

    if args.jobs <= 0:
        args.jobs = 1 if args.mode == "time" else performance_cores()
    if args.mode == "time" and args.jobs > 1:
        print(
            f"warning: --mode time with --jobs {args.jobs}; concurrent runs distort timings",
            file=sys.stderr,
        )

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
        writer.writerow(
            ["solver", "suite", "instance", "status", "seconds", "peak_kb", *METRIC_FIELDS]
        )
        for r in sorted(results, key=lambda r: (r.suite, r.instance, r.solver)):
            writer.writerow([
                r.solver, r.suite, r.instance, r.status, f"{r.seconds:.6f}", r.peak_kb,
                *(r.metrics.get(f, "") for f in METRIC_FIELDS),
            ])
    print(f"wrote {args.out}", file=sys.stderr)

    problems, ours = check_agreement(results)
    if problems:
        print("\n!! VERDICT DISAGREEMENTS !!", file=sys.stderr)
        for p in problems:
            marker = "  <-- dpbst in the minority" if p in ours else ""
            print("  " + p + marker, file=sys.stderr)
        if not ours:
            print("  (dpbst agreed with the majority everywhere)", file=sys.stderr)

    if args.mode in ("time", "metrics"):
        table = (
            summarize(results, args.solvers, args.timeout)
            if args.mode == "time"
            else summarize_metrics(results, args.solvers)
        )
        print(table)
        if args.markdown:
            args.markdown.parent.mkdir(parents=True, exist_ok=True)
            args.markdown.write_text(table + "\n")
    else:
        solved = sum(1 for r in results if r.status in ("SAT", "UNSAT"))
        print(f"{solved}/{len(results)} runs produced a verdict; "
              f"{len(problems)} disagreement(s), {len(ours)} involving dpbst")

    # Only a disagreement that dpbst is on the wrong side of is this project's failure.
    return 1 if ours else 0


if __name__ == "__main__":
    raise SystemExit(main())
