# /// script
# requires-python = ">=3.11,<3.14"
# dependencies = ["matplotlib>=3.8", "pandas>=2.1", "numpy>=1.26"]
# ///
"""Generate publication-quality figures and table fragments for the DP-BST paper.

Reads the CSV files produced by scripts/bench.py (results/hard.csv, results/buckets.csv,
results/ablation.csv, results/heuristics.csv) and writes one PDF per figure to paper/figures/
and one LaTeX fragment per table to paper/tables/. The paper (paper/paper.tex) includes both
sets of files directly, so a figure or a number in the paper is never hand transcribed from a
spreadsheet: it is regenerated from the same CSV every time this script runs.

Run with uv, which resolves matplotlib, pandas and numpy into a disposable environment:

    uv run --script scripts/academic_plots.py

Two policies are enforced throughout, both load bearing for the paper's honesty:

1. Wall clock comparisons are read only from results/hard.csv, which scripts/bench.py always
   produces with --mode time --jobs 1. The other three CSVs are produced with --mode metrics
   and may have been collected under parallel execution; only their deterministic counters
   (search nodes, cache hit rate, bucket depth) are used here, never their seconds column.
2. Unsolved instances (timeout or parse error) are never silently dropped from a mean. They are
   excluded from time-based statistics but counted explicitly, and the standard PAR-2 score
   (park.wallclock time for solved instances, twice the time limit for unsolved ones) is reported
   alongside raw solve counts so that a solver cannot improve its apparent average by timing out
   on the instances it would have been slowest on.
"""

from __future__ import annotations

import pathlib
import sys

import matplotlib
import numpy as np
import pandas as pd

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import matplotlib.ticker as mticker  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parent.parent
RESULTS = ROOT / "results"
FIGURES = ROOT / "paper" / "figures"
TABLES = ROOT / "paper" / "tables"

SOLVED = ("SAT", "UNSAT")
TIMEOUT_LIMIT_S = 10.0  # matches --timeout 10 in scripts/run_all_benchmarks.sh

# Display names and the reference tier each solver represents, used throughout the paper.
SOLVER_INFO = {
    "dpbst": ("DP-BST", "this work"),
    "kissat": ("Kissat", "state of the art"),
    "cadical": ("CaDiCaL", "standard"),
    "minisat": ("MiniSat", "established"),
    "varisat": ("varisat", "Rust, established"),
    "splr": ("splr", "Rust, established"),
}
SOLVER_ORDER = ["dpbst", "kissat", "cadical", "minisat", "varisat", "splr"]

# A fixed, colourblind-safe qualitative palette (Wong, 2011, Nature Methods), paired with
# distinct markers and line styles so the figures remain legible if printed without colour.
COLOUR = {
    "dpbst": "#D55E00", "kissat": "#0072B2", "cadical": "#009E73",
    "minisat": "#CC79A7", "varisat": "#E69F00", "splr": "#56B4E9",
}
MARKER = {"dpbst": "o", "kissat": "s", "cadical": "^", "minisat": "D", "varisat": "v", "splr": "P"}
LINESTYLE = {
    "dpbst": "-", "kissat": "--", "cadical": "-.", "minisat": ":", "varisat": (0, (3, 1, 1, 1)),
    "splr": (0, (5, 1)),
}


def style() -> None:
    """A conservative, print-oriented rcParams profile.

    Serif text to match a typeset paper, vector output, no figure titles (captions are set by
    LaTeX, not baked into the raster), and light horizontal gridlines only.
    """
    plt.rcParams.update({
        "font.family": "serif",
        "mathtext.fontset": "dejavuserif",
        "figure.dpi": 300,
        "savefig.dpi": 300,
        "pdf.fonttype": 42,
        "axes.grid": True,
        "axes.grid.axis": "y",
        "grid.alpha": 0.3,
        "grid.linewidth": 0.5,
        "axes.spines.top": False,
        "axes.spines.right": False,
        "axes.titlesize": 0,
        "axes.labelsize": 9,
        "xtick.labelsize": 8,
        "ytick.labelsize": 8,
        "legend.fontsize": 7.5,
        "legend.frameon": False,
        "font.size": 9,
        "lines.linewidth": 1.3,
        "lines.markersize": 4.0,
    })


def load_csv(name: str) -> pd.DataFrame:
    path = RESULTS / name
    if not path.is_file():
        sys.exit(f"missing {path}; run scripts/run_all_benchmarks.sh first")
    frame = pd.read_csv(path)
    numeric = [
        "seconds", "peak_kb", "nodes", "decisions", "conflicts", "propagations", "components",
        "cache_lookups", "cache_hits", "cache_hit_rate", "cache_entries", "table_max_bucket",
        "table_max_depth", "table_mean_depth",
    ]
    for column in numeric:
        if column in frame.columns:
            frame[column] = pd.to_numeric(frame[column], errors="coerce")
    return frame


def par2(seconds: pd.Series, status: pd.Series, limit: float) -> float:
    """PAR-2: the mean of the actual time for solved instances and twice the limit for the rest.

    The standard scoring rule of the SAT Competition. It is used here instead of a plain mean
    because a plain mean over solved instances only would let a solver improve its reported
    average by timing out on the hardest cases it would otherwise have been slowest on.
    """
    penalised = np.where(status.isin(SOLVED), seconds, 2.0 * limit)
    return float(np.mean(penalised))


def save(fig: plt.Figure, name: str) -> None:
    FIGURES.mkdir(parents=True, exist_ok=True)
    fig.tight_layout(pad=0.4)
    path = FIGURES / f"{name}.pdf"
    fig.savefig(path, bbox_inches="tight")
    plt.close(fig)
    print(f"  wrote {path.relative_to(ROOT)}")


def write_table(name: str, tex: str) -> None:
    TABLES.mkdir(parents=True, exist_ok=True)
    path = TABLES / f"{name}.tex"
    path.write_text(tex)
    print(f"  wrote {path.relative_to(ROOT)}")


def escape(text: str) -> str:
    for old, new in (("_", r"\_"), ("&", r"\&"), ("%", r"\%"), ("#", r"\#")):
        text = text.replace(old, new)
    return text


# -------------------------------------------------------------------------------------------
# Figure 1: cactus plot
# -------------------------------------------------------------------------------------------

def fig_cactus(timed: pd.DataFrame) -> None:
    """Instances solved as a function of cumulative wall clock time.

    The standard comparison plot of the SAT Competition and of the wider empirical algorithms
    literature (Hoos and Stutzle, 2004). A curve further to the right solves more instances
    within a given time budget; a curve lower down solves the same instances faster. Unlike a
    mean or a median, it does not let a single pathological instance dominate the comparison,
    and it makes partial failure (timeouts) visible rather than averaging over it.
    """
    fig, ax = plt.subplots(figsize=(7.0, 3.4))
    for solver in SOLVER_ORDER:
        subset = timed[timed.solver == solver]
        if subset.empty:
            continue
        solved = subset[subset.status.isin(SOLVED)]["seconds"].dropna().sort_values()
        if solved.empty:
            continue
        name, _ = SOLVER_INFO[solver]
        ax.plot(
            range(1, len(solved) + 1), solved.to_numpy(),
            label=name, color=COLOUR[solver], marker=MARKER[solver],
            linestyle=LINESTYLE[solver], markevery=max(len(solved) // 12, 1), markersize=3.6,
        )
    ax.set_yscale("log")
    ax.set_xlabel("instances solved, ranked by solve time")
    ax.set_ylabel("solve time (s, log scale)")
    ax.legend(ncol=3, loc="upper left")
    ax.yaxis.set_major_formatter(mticker.FuncFormatter(lambda v, _: f"{v:g}"))
    save(fig, "fig_cactus")


# -------------------------------------------------------------------------------------------
# Figure 2: head to head scatter against the state of the art
# -------------------------------------------------------------------------------------------

def fig_scatter(timed: pd.DataFrame, ours: str = "dpbst", theirs: str = "kissat") -> None:
    a = timed[timed.solver == ours].set_index(["suite", "instance"])
    b = timed[timed.solver == theirs].set_index(["suite", "instance"])
    common = a.index.intersection(b.index)
    if len(common) < 3:
        return

    floor = 1e-3
    x = b.loc[common, "seconds"].clip(lower=floor)
    y = a.loc[common, "seconds"].clip(lower=floor)
    both = a.loc[common, "status"].isin(SOLVED) & b.loc[common, "status"].isin(SOLVED)

    fig, ax = plt.subplots(figsize=(3.4, 3.3))
    ax.scatter(x[both], y[both], s=10, alpha=0.65, color=COLOUR["dpbst"],
               edgecolors="none", label="both solved", zorder=3)
    if (~both).any():
        ax.scatter(x[~both], y[~both], s=14, alpha=0.85, color="0.4", marker="x",
                   label="one timed out", zorder=3)

    lo = float(min(x.min(), y.min())) * 0.6
    hi = float(max(x.max(), y.max())) * 1.6
    ax.plot([lo, hi], [lo, hi], color="0.3", linewidth=0.9, linestyle="--", zorder=2)
    for factor in (10, 100):
        ax.plot([lo, hi], [lo * factor, hi * factor], color="0.75", linewidth=0.6,
                 linestyle=":", zorder=1)

    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.set_xlim(lo, hi)
    ax.set_ylim(lo, hi)
    ax.set_aspect("equal")
    ax.set_xlabel(f"{SOLVER_INFO[theirs][0]} time (s)")
    ax.set_ylabel(f"{SOLVER_INFO[ours][0]} time (s)")
    ax.legend(loc="lower right", fontsize=6.8)
    save(fig, "fig_scatter")


# -------------------------------------------------------------------------------------------
# Figure 3: per-family median time, all solvers
# -------------------------------------------------------------------------------------------

def fig_family_bars(timed: pd.DataFrame) -> None:
    families = sorted(timed["suite"].unique())
    fig, ax = plt.subplots(figsize=(7.0, 3.3))
    n = len(SOLVER_ORDER)
    width = 0.8 / n
    positions = np.arange(len(families))
    for i, solver in enumerate(SOLVER_ORDER):
        medians = []
        for family in families:
            subset = timed[(timed.suite == family) & (timed.solver == solver)]
            solved = subset[subset.status.isin(SOLVED)]["seconds"]
            medians.append(solved.median() if len(solved) else np.nan)
        offsets = positions - 0.4 + width * (i + 0.5)
        ax.bar(offsets, medians, width, label=SOLVER_INFO[solver][0], color=COLOUR[solver])
    ax.set_yscale("log")
    ax.set_xticks(positions)
    ax.set_xticklabels([escape(f) for f in families], rotation=32, ha="right")
    ax.set_ylabel("median solve time (s, log scale)")
    ax.legend(ncol=3, loc="upper left", fontsize=6.8)
    save(fig, "fig_family_bars")


# -------------------------------------------------------------------------------------------
# Figure 4: peak memory
# -------------------------------------------------------------------------------------------

def fig_memory(timed: pd.DataFrame) -> None:
    fig, ax = plt.subplots(figsize=(3.4, 3.0))
    data, labels, colours = [], [], []
    for solver in SOLVER_ORDER:
        values = timed[(timed.solver == solver)]["peak_kb"].dropna() / 1024.0
        if values.empty:
            continue
        data.append(values.to_numpy())
        labels.append(SOLVER_INFO[solver][0])
        colours.append(COLOUR[solver])
    bp = ax.boxplot(data, tick_labels=labels, showfliers=False, patch_artist=True, widths=0.55)
    for patch, colour in zip(bp["boxes"], colours):
        patch.set_facecolor(colour)
        patch.set_alpha(0.55)
    ax.set_yscale("log")
    ax.set_ylabel("peak resident memory (MiB, log scale)")
    plt.setp(ax.get_xticklabels(), rotation=28, ha="right")
    save(fig, "fig_memory")


# -------------------------------------------------------------------------------------------
# Figure 5: memoisation hit rate by instance family
# -------------------------------------------------------------------------------------------

def fig_hitrate(all_metrics: pd.DataFrame) -> None:
    rows = all_metrics[(all_metrics.solver == "dpbst-avl") & all_metrics.cache_lookups.notna()]
    if rows.empty:
        return
    grouped = rows.groupby("suite")[["cache_lookups", "cache_hits"]].sum()
    grouped = grouped[grouped.cache_lookups > 0]
    if grouped.empty:
        return
    rate = (grouped.cache_hits / grouped.cache_lookups * 100).sort_values()

    fig, ax = plt.subplots(figsize=(3.4, 0.3 * len(rate) + 1.1))
    colours = [COLOUR["dpbst"] if v >= 5 else "0.6" for v in rate]
    ax.barh([escape(i) for i in rate.index], rate.to_numpy(), color=colours, height=0.65)
    for y, v in enumerate(rate.to_numpy()):
        ax.text(v + 1.0, y, f"{v:.1f}", va="center", fontsize=6.8)
    ax.set_xlabel("memoisation hit rate (\\%)")
    ax.set_xlim(0, max(rate.max() * 1.22, 8))
    ax.grid(axis="x", alpha=0.25)
    ax.grid(axis="y", visible=False)
    save(fig, "fig_hitrate")


# -------------------------------------------------------------------------------------------
# Figure 6: bucket policy, mean and maximum depth
# -------------------------------------------------------------------------------------------

def fig_bucket_depth(buckets: pd.DataFrame) -> None:
    rows = buckets[buckets.table_mean_depth.notna() & (buckets.table_mean_depth > 0)]
    if rows.empty:
        return
    stats = rows.groupby("solver")[["table_mean_depth", "table_max_depth"]].mean()
    order = ["dpbst-avl", "dpbst-unbalanced", "dpbst-splay", "dpbst-chain"]
    stats = stats.reindex([s for s in order if s in stats.index])
    labels = [s.replace("dpbst-", "") for s in stats.index]

    fig, ax = plt.subplots(figsize=(3.4, 3.0))
    positions = np.arange(len(stats))
    width = 0.36
    ax.bar(positions - width / 2, stats.table_mean_depth, width,
           label="mean depth", color=COLOUR["dpbst"])
    ax.bar(positions + width / 2, stats.table_max_depth, width,
           label="maximum depth", color="0.55")
    ax.set_xticks(positions)
    ax.set_xticklabels(labels, rotation=20, ha="right")
    ax.set_ylabel("comparisons per bucket lookup")
    ax.legend()
    save(fig, "fig_bucket_depth")


# -------------------------------------------------------------------------------------------
# Figure 7: ablation, search nodes by configuration
# -------------------------------------------------------------------------------------------

def fig_ablation(ablation: pd.DataFrame) -> None:
    rows = ablation[ablation.nodes.notna()]
    if rows.empty:
        return
    pivot = rows.pivot_table(index="suite", columns="solver", values="nodes", aggfunc="sum")
    order = ["dpbst", "dpbst-nocache", "dpbst-nopure", "dpbst-nopre", "dpbst-plain-dpll"]
    pivot = pivot.reindex(columns=[c for c in order if c in pivot.columns])
    labels = {
        "dpbst": "full", "dpbst-nocache": "no memo", "dpbst-nopure": "no pure literals",
        "dpbst-nopre": "no preprocessing", "dpbst-plain-dpll": "plain DPLL",
    }

    fig, ax = plt.subplots(figsize=(7.0, 3.2))
    n = len(pivot.columns)
    width = 0.8 / max(n, 1)
    positions = np.arange(len(pivot.index))
    greys = plt.cm.Greys(np.linspace(0.35, 0.85, n))
    for i, column in enumerate(pivot.columns):
        offsets = positions - 0.4 + width * (i + 0.5)
        colour = COLOUR["dpbst"] if column == "dpbst" else greys[i]
        ax.bar(offsets, pivot[column].fillna(0), width, label=labels[column], color=colour)
    ax.set_yscale("log")
    ax.set_xticks(positions)
    ax.set_xticklabels([escape(s) for s in pivot.index], rotation=28, ha="right")
    ax.set_ylabel("search nodes, summed over instances (log scale)")
    ax.legend(ncol=3, loc="upper left", fontsize=6.8)
    save(fig, "fig_ablation")


# -------------------------------------------------------------------------------------------
# Tables
# -------------------------------------------------------------------------------------------

def table_par2(timed: pd.DataFrame) -> None:
    rows = []
    for solver in SOLVER_ORDER:
        subset = timed[timed.solver == solver]
        if subset.empty:
            continue
        name, tier = SOLVER_INFO[solver]
        solved = int(subset.status.isin(SOLVED).sum())
        total = len(subset)
        score = par2(subset["seconds"], subset["status"], TIMEOUT_LIMIT_S)
        rows.append((score, name, tier, solved, total, score))

    rows.sort(key=lambda r: r[0])
    lines = []
    for _, name, tier, solved, total, score in rows:
        lines.append(f"{name} & {tier} & {solved}/{total} & {score:.2f} \\\\")
    write_table("tab_par2", (
        "\\begin{tabular}{llrr}\n\\toprule\n"
        "solver & tier & solved & PAR-2 (s) \\\\\n\\midrule\n"
        + "\n".join(lines) + "\n\\bottomrule\n\\end{tabular}\n"
    ))


def table_families(timed: pd.DataFrame) -> None:
    descriptions = {
        "aim": "artificial, planted small backbone",
        "bf": "circuit fault detection",
        "blocks": "independent random 3-SAT blocks",
        "chain": "random 3-SAT blocks joined in a path",
        "dubois": "generator gensathard, unsatisfiable",
        "grid": "grid graph colouring",
        "jnh": "random, mixed clause length",
        "parity": "parity learning",
        "php": "pigeonhole, unsatisfiable",
        "QG": "quasigroup existence",
        "rand3": "uniform random 3-SAT, ratio 4.26",
        "ssa": "single stuck-at circuit fault",
        "uf250-1065": "uniform random 3-SAT, satisfiable",
        "uuf250-1065": "uniform random 3-SAT, unsatisfiable",
    }
    lines = []
    for family in sorted(timed["suite"].unique()):
        n = timed[timed.suite == family]["instance"].nunique()
        desc = descriptions.get(family, "")
        lines.append(f"\\texttt{{{escape(family)}}} & {n} & {desc} \\\\")
    write_table("tab_families", (
        "\\begin{tabular}{lrl}\n\\toprule\n"
        "family & instances & description \\\\\n\\midrule\n"
        + "\n".join(lines) + "\n\\bottomrule\n\\end{tabular}\n"
    ))


def table_bucket_summary(buckets: pd.DataFrame) -> None:
    rows = buckets[buckets.table_mean_depth.notna() & (buckets.table_mean_depth > 0)]
    order = ["dpbst-avl", "dpbst-unbalanced", "dpbst-splay", "dpbst-chain"]
    lines = []
    for solver in order:
        subset = rows[rows.solver == solver]
        if subset.empty:
            continue
        mean_depth = subset.table_mean_depth.mean()
        max_depth = subset.table_max_depth.mean()
        entries = subset.cache_entries.mean()
        lines.append(
            f"\\texttt{{{solver.replace('dpbst-', '')}}} & {mean_depth:.2f} & "
            f"{max_depth:.1f} & {entries:,.0f} \\\\"
        )
    write_table("tab_buckets", (
        "\\begin{tabular}{lrrr}\n\\toprule\n"
        "policy & mean depth & max depth & mean entries \\\\\n\\midrule\n"
        + "\n".join(lines) + "\n\\bottomrule\n\\end{tabular}\n"
    ))


def table_versions() -> None:
    import subprocess

    def run(*cmd: str) -> str:
        try:
            return subprocess.run(cmd, capture_output=True, text=True, check=True).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return "unknown"

    facts = [
        ("processor", run("sysctl", "-n", "machdep.cpu.brand_string")),
        ("performance cores", run("sysctl", "-n", "hw.perflevel0.logicalcpu")),
        ("operating system",
         f'{run("sw_vers", "-productName")} {run("sw_vers", "-productVersion")}'),
        ("Rust compiler", run("rustc", "--version")),
        ("time limit per instance", f"{TIMEOUT_LIMIT_S:g} s"),
    ]
    lines = [f"{escape(k)} & {escape(v)} \\\\" for k, v in facts if v and v != "unknown"]
    write_table("tab_setup", (
        "\\begin{tabular}{ll}\n\\toprule\n"
        "parameter & value \\\\\n\\midrule\n"
        + "\n".join(lines) + "\n\\bottomrule\n\\end{tabular}\n"
    ))


def main() -> int:
    style()
    print("loading data")
    timed = load_csv("hard.csv")
    buckets = load_csv("buckets.csv")
    ablation = load_csv("ablation.csv")

    print("figures")
    fig_cactus(timed)
    fig_scatter(timed)
    fig_family_bars(timed)
    fig_memory(timed)
    fig_hitrate(pd.concat([buckets], ignore_index=True))
    fig_bucket_depth(buckets)
    fig_ablation(ablation)

    print("tables")
    table_par2(timed)
    table_families(timed)
    table_bucket_summary(buckets)
    table_versions()

    print("done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
