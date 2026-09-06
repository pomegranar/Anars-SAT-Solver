# /// script
# requires-python = ">=3.11,<3.14"
# dependencies = ["matplotlib>=3.8", "pandas>=2.1"]
# ///
"""Turn the benchmark CSVs into a typeset PDF report.

Run with uv, which resolves the dependencies into a throwaway environment:

    uv run --script scripts/report.py

Figures are matplotlib; the document is LaTeX, compiled with latexmk.

One rule is enforced throughout: **wall-clock times are only ever read from the sequential
run.** `scripts/bench.py --mode metrics` runs in parallel, so its `seconds` column is polluted by
contention and by macOS scheduling work onto efficiency cores. Those CSVs still carry the
solver's own deterministic counters -- search nodes, memo hit rate, bucket depth -- which are
identical however loaded the machine was, and only those are plotted from them.
"""

from __future__ import annotations

import argparse
import datetime as dt
import pathlib
import shutil
import subprocess
import sys

import matplotlib
import pandas as pd

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Only this file was produced by a sequential, one-job-at-a-time run.
TIMED_CSV = "hard.csv"
SOLVED = ("SAT", "UNSAT")

# Colourblind-safe qualitative palette (Okabe-Ito).
PALETTE = ["#0072B2", "#D55E00", "#009E73", "#CC79A7", "#E69F00", "#56B4E9", "#F0E442", "#000000"]


def style() -> None:
    plt.rcParams.update({
        "figure.figsize": (7.0, 4.0),
        "figure.dpi": 150,
        "axes.grid": True,
        "axes.grid.axis": "y",
        "grid.alpha": 0.25,
        "grid.linewidth": 0.6,
        "axes.spines.top": False,
        "axes.spines.right": False,
        "axes.titlesize": 11,
        "axes.titleweight": "bold",
        "axes.labelsize": 9,
        "xtick.labelsize": 8,
        "ytick.labelsize": 8,
        "legend.fontsize": 8,
        "legend.frameon": False,
        "font.size": 9,
    })


def load(name: str) -> pd.DataFrame | None:
    path = ROOT / "results" / name
    if not path.is_file():
        return None
    frame = pd.read_csv(path)
    if frame.empty:
        return None
    for column in ("seconds", "nodes", "components", "cache_lookups", "cache_hits",
                   "cache_entries", "table_max_bucket", "table_max_depth", "table_mean_depth",
                   "propagations", "conflicts", "decisions", "peak_kb"):
        if column in frame.columns:
            frame[column] = pd.to_numeric(frame[column], errors="coerce")
    return frame


def escape(text: str) -> str:
    """Escapes the LaTeX specials that appear in solver and suite names."""
    for old, new in (("\\", r"\textbackslash{}"), ("_", r"\_"), ("&", r"\&"), ("%", r"\%"),
                     ("#", r"\#"), ("$", r"\$"), ("{", r"\{"), ("}", r"\}"), ("^", r"\^{}"),
                     ("~", r"\textasciitilde{}")):
        text = text.replace(old, new)
    return text


def save(fig: plt.Figure, out: pathlib.Path, name: str) -> str:
    fig.tight_layout()
    path = out / f"{name}.pdf"
    fig.savefig(path, bbox_inches="tight")
    plt.close(fig)
    return path.name


# ---------------------------------------------------------------------------------------------
# Figures
# ---------------------------------------------------------------------------------------------

def cactus_plot(timed: pd.DataFrame, out: pathlib.Path) -> str | None:
    """The SAT competition's standard plot: instances solved against cumulative time.

    A curve further right solves more; a curve lower down solves them faster. It compares
    solvers over a whole instance set without letting one pathological instance dominate a mean.
    """
    fig, ax = plt.subplots()
    plotted = False
    for i, solver in enumerate(sorted(timed["solver"].unique())):
        times = timed[(timed.solver == solver) & (timed.status.isin(SOLVED))]["seconds"]
        times = times.dropna().sort_values()
        if times.empty:
            continue
        ax.plot(range(1, len(times) + 1), times.cumsum(),
                label=solver, color=PALETTE[i % len(PALETTE)], linewidth=1.6)
        plotted = True
    if not plotted:
        plt.close(fig)
        return None
    ax.set_yscale("log")
    ax.set_xlabel("instances solved")
    ax.set_ylabel("cumulative seconds (log)")
    ax.set_title("Instances solved against cumulative time")
    ax.legend(ncol=2)
    return save(fig, out, "cactus")


def head_to_head(timed: pd.DataFrame, out: pathlib.Path, ours="dpbst", theirs="kissat") -> str | None:
    """Per-instance scatter against a reference solver, log-log."""
    a = timed[timed.solver == ours].set_index("instance")
    b = timed[timed.solver == theirs].set_index("instance")
    common = a.index.intersection(b.index)
    if len(common) < 3:
        return None

    floor = 1e-4  # below this, we are timing process start-up, not solving
    x = b.loc[common, "seconds"].clip(lower=floor)
    y = a.loc[common, "seconds"].clip(lower=floor)
    both_solved = (a.loc[common, "status"].isin(SOLVED)) & (b.loc[common, "status"].isin(SOLVED))

    fig, ax = plt.subplots(figsize=(4.6, 4.4))
    ax.scatter(x[both_solved], y[both_solved], s=14, alpha=0.75,
               color=PALETTE[0], edgecolors="none", label="both solved")
    if (~both_solved).any():
        ax.scatter(x[~both_solved], y[~both_solved], s=16, alpha=0.85, color=PALETTE[1],
                   marker="x", label="a timeout")

    lo = float(min(x.min(), y.min())) * 0.7
    hi = float(max(x.max(), y.max())) * 1.4
    ax.plot([lo, hi], [lo, hi], color="0.35", linewidth=0.9, linestyle="--", label="equal")
    for factor, label in ((10, r"10$\times$"), (100, r"100$\times$")):
        ax.plot([lo, hi], [lo * factor, hi * factor], color="0.7", linewidth=0.7, linestyle=":")
        ax.annotate(label, (hi * 0.35, hi * 0.35 * factor), fontsize=7, color="0.45")

    ax.set_xscale("log"); ax.set_yscale("log")
    ax.set_xlim(lo, hi); ax.set_ylim(lo, hi)
    ax.set_xlabel(f"{theirs} seconds (log)")
    ax.set_ylabel(f"{ours} seconds (log)")
    ax.set_title(f"{ours} against {theirs}, per instance")
    ax.grid(True, which="both", alpha=0.2)
    ax.legend(loc="lower right")
    return save(fig, out, "head_to_head")


def hit_rate_by_suite(timed: pd.DataFrame, out: pathlib.Path) -> str | None:
    """Where the memo actually earns its keep."""
    ours = timed[(timed.solver == "dpbst") & timed["cache_lookups"].notna()]
    if ours.empty:
        return None
    grouped = ours.groupby("suite")[["cache_lookups", "cache_hits"]].sum()
    grouped = grouped[grouped.cache_lookups > 0]
    if grouped.empty:
        return None
    rate = (grouped.cache_hits / grouped.cache_lookups * 100).sort_values()

    fig, ax = plt.subplots(figsize=(7.0, 0.34 * len(rate) + 1.4))
    colours = [PALETTE[2] if v >= 10 else PALETTE[1] for v in rate]
    ax.barh([escape(i) for i in rate.index], rate.values, color=colours, height=0.68)
    for y, v in enumerate(rate.values):
        ax.text(v + 0.7, y, f"{v:.1f}%", va="center", fontsize=7.5)
    ax.set_xlabel("memo hit rate (%)")
    ax.set_title("Memo hit rate by instance family")
    ax.grid(axis="x", alpha=0.25); ax.grid(axis="y", visible=False)
    ax.set_xlim(0, max(rate.max() * 1.18, 5))
    return save(fig, out, "hit_rate")


def bucket_depth(buckets: pd.DataFrame, out: pathlib.Path) -> str | None:
    """The project's central question, measured structurally rather than by wall clock.

    Mean depth is the expected number of comparisons a successful lookup performs, and it is
    deterministic, so it is meaningful even though these runs were executed in parallel.
    """
    rows = buckets[buckets["table_mean_depth"].notna() & (buckets["table_mean_depth"] > 0)]
    if rows.empty:
        return None
    stats = rows.groupby("solver")[["table_mean_depth", "table_max_depth"]].mean()
    stats = stats.sort_values("table_mean_depth")

    fig, ax = plt.subplots(figsize=(6.4, 3.4))
    positions = range(len(stats))
    width = 0.38
    ax.bar([p - width / 2 for p in positions], stats["table_mean_depth"], width,
           label="mean depth", color=PALETTE[0])
    ax.bar([p + width / 2 for p in positions], stats["table_max_depth"], width,
           label="max depth", color=PALETTE[4])
    ax.set_xticks(list(positions))
    ax.set_xticklabels([escape(s.replace("dpbst-", "")) for s in stats.index])
    ax.set_ylabel("bucket depth (nodes)")
    ax.set_title("Bucket depth by policy (lower is fewer comparisons per lookup)")
    ax.legend()
    return save(fig, out, "bucket_depth")


def ablation_nodes(ablation: pd.DataFrame, out: pathlib.Path) -> str | None:
    """Search nodes per configuration: what each technique removes from the search."""
    rows = ablation[ablation["nodes"].notna()]
    if rows.empty:
        return None
    pivot = rows.pivot_table(index="suite", columns="solver", values="nodes", aggfunc="sum")
    pivot = pivot.dropna(axis=1, how="all")
    if pivot.empty:
        return None

    fig, ax = plt.subplots(figsize=(7.2, 3.8))
    n = len(pivot.columns)
    width = 0.8 / max(n, 1)
    for i, column in enumerate(pivot.columns):
        offsets = [x - 0.4 + width * (i + 0.5) for x in range(len(pivot.index))]
        ax.bar(offsets, pivot[column].fillna(0), width,
               label=escape(column.replace("dpbst-", "")), color=PALETTE[i % len(PALETTE)])
    ax.set_yscale("log")
    ax.set_xticks(range(len(pivot.index)))
    ax.set_xticklabels([escape(s) for s in pivot.index], rotation=20, ha="right")
    ax.set_ylabel("search nodes, summed (log)")
    ax.set_title("Search nodes by configuration")
    ax.legend(ncol=3)
    return save(fig, out, "ablation")


def heuristic_nodes(heuristics: pd.DataFrame, out: pathlib.Path) -> str | None:
    rows = heuristics[heuristics["nodes"].notna()]
    if rows.empty:
        return None
    totals = rows.groupby("solver")["nodes"].sum().sort_values()
    fig, ax = plt.subplots(figsize=(6.0, 3.2))
    ax.bar([escape(s.replace("dpbst-", "")) for s in totals.index], totals.values,
           color=PALETTE[0], width=0.6)
    ax.set_yscale("log")
    ax.set_ylabel("search nodes, summed (log)")
    ax.set_title("Branching heuristics: search nodes")
    return save(fig, out, "heuristics")


# ---------------------------------------------------------------------------------------------
# Tables
# ---------------------------------------------------------------------------------------------

def timing_table(timed: pd.DataFrame) -> str:
    rows = []
    for suite in sorted(timed["suite"].unique()):
        subset = timed[timed.suite == suite]
        total = len(subset["instance"].unique())
        first = True
        for solver in sorted(subset["solver"].unique(),
                             key=lambda s: subset[subset.solver == s]["seconds"].sum()):
            runs = subset[subset.solver == solver]
            solved = runs[runs.status.isin(SOLVED)]
            rows.append(
                f"{escape(suite) if first else ''} & \\texttt{{{escape(solver)}}} & "
                f"{len(solved)}/{total} & {runs['seconds'].sum():.2f} & "
                f"{solved['seconds'].median() if len(solved) else float('nan'):.4f} & "
                f"{runs['seconds'].max():.2f} \\\\"
            )
            first = False
        rows.append(r"\addlinespace")
    body = "\n".join(rows)
    return (
        "\\begin{longtable}{llrrrr}\n\\toprule\n"
        "family & solver & solved & total (s) & median (s) & max (s) \\\\\n"
        "\\midrule\n\\endhead\n" + body + "\n\\bottomrule\n\\end{longtable}\n"
    )


def machine_facts() -> list[tuple[str, str]]:
    def run(*cmd: str) -> str:
        try:
            return subprocess.run(cmd, capture_output=True, text=True, check=True).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return "unknown"

    facts = [
        ("generated", dt.datetime.now().astimezone().strftime("%Y-%m-%d %H:%M %Z")),
        ("cpu", run("sysctl", "-n", "machdep.cpu.brand_string")),
        ("cores", f'{run("sysctl", "-n", "hw.perflevel0.logicalcpu")} performance, '
                  f'{run("sysctl", "-n", "hw.perflevel1.logicalcpu")} efficiency'),
        ("os", f'{run("sw_vers", "-productName")} {run("sw_vers", "-productVersion")}'),
        ("rustc", run("rustc", "--version")),
        ("commit", run("git", "-C", str(ROOT), "rev-parse", "--short", "HEAD")),
    ]
    return [(k, v) for k, v in facts if v and v != "unknown"]


# ---------------------------------------------------------------------------------------------

DOCUMENT = r"""\documentclass[10pt,a4paper]{article}
\usepackage[margin=2cm]{geometry}
\usepackage{graphicx,booktabs,longtable,xcolor,hyperref}
\usepackage[T1]{fontenc}
\hypersetup{colorlinks=true, linkcolor=black, urlcolor=blue!60!black}
\setlength{\parindent}{0pt}
\setlength{\parskip}{0.6em}
\title{\vspace{-1.5cm}\textbf{DP-BST benchmark report}\\[0.2em]
       \large A SAT solver on DPLL, component caching, and a hash table of binary search trees}
\date{}
\begin{document}
\maketitle
\vspace{-2.2em}
%(facts)s
\hrule
%(body)s
\end{document}
"""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=pathlib.Path, default=ROOT / "results" / "report.pdf")
    args = parser.parse_args()

    style()
    build = ROOT / "results" / "_report"
    build.mkdir(parents=True, exist_ok=True)

    timed = load(TIMED_CSV)
    buckets = load("buckets.csv")
    ablation = load("ablation.csv")
    heuristics = load("heuristics.csv")

    if timed is None:
        print("no results/hard.csv; run scripts/run_all_benchmarks.sh first", file=sys.stderr)
        return 1

    sections: list[str] = []

    def figure(path: str | None, caption: str) -> None:
        if path:
            sections.append(
                f"\\begin{{center}}\\includegraphics[width=\\linewidth]{{{path}}}\\end{{center}}\n"
                f"\\small {caption}\\normalsize\n"
            )

    sections.append(r"\section{How dpbst compares}")
    sections.append(
        "Timings below come from the sequential stage of the harness, one solver at a time. "
        "Every other measurement in this report is a deterministic counter reported by the "
        "solver itself, which is unaffected by machine load and so was gathered in parallel.")
    figure(cactus_plot(timed, build),
           "Further right solves more instances; lower solves them faster.")
    figure(head_to_head(timed, build),
           "Points above the dashed line are instances Kissat wins. The dotted guides mark "
           "$10\\times$ and $100\\times$.")

    sections.append(r"\section{Where the memo pays}")
    figure(hit_rate_by_suite(timed, build),
           "Structured families reuse subproblems; random 3-SAT never decomposes, so the memo "
           "there is pure overhead. This was predicted in \\texttt{docs/DESIGN.md} before the "
           "solver was written.")

    if buckets is not None:
        sections.append(r"\section{Bucket policy}")
        figure(bucket_depth(buckets, build),
               "Mean depth is the expected number of comparisons for a successful lookup.")

    if ablation is not None:
        sections.append(r"\section{Ablation}")
        figure(ablation_nodes(ablation, build), "Lower is better; note the logarithmic axis.")

    if heuristics is not None:
        sections.append(r"\section{Branching heuristics}")
        figure(heuristic_nodes(heuristics, build), "Summed over the families where the memo runs.")

    sections.append(r"\section{Full timing table}")
    sections.append(timing_table(timed))

    facts = "\\begin{tabular}{ll}\n" + "\n".join(
        f"\\textbf{{{escape(k)}}} & {escape(v)} \\\\" for k, v in machine_facts()
    ) + "\n\\end{tabular}\n"

    tex = build / "report.tex"
    tex.write_text(DOCUMENT % {"facts": facts, "body": "\n".join(sections)})

    result = subprocess.run(
        ["latexmk", "-pdf", "-interaction=nonstopmode", "-halt-on-error", "report.tex"],
        cwd=build, capture_output=True, text=True,
    )
    produced = build / "report.pdf"
    if result.returncode != 0 or not produced.is_file():
        print(result.stdout[-3000:], file=sys.stderr)
        print("latexmk failed", file=sys.stderr)
        return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy(produced, args.out)
    print(f"wrote {args.out} ({args.out.stat().st_size // 1024} KiB)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
