#!/usr/bin/env bash
# Reproduce every measurement quoted in the README.
#
# Only stage 1 measures wall-clock time across different solvers, and only stage 1 runs
# sequentially. Everything after it either reports the solver's own deterministic counters --
# search nodes, memo hit rate, bucket depth, all identical however loaded the machine is -- or
# checks verdicts, so those stages run across the performance cores.
#
# Bucket-policy *timing* is measured by criterion (stage 6) rather than by wall-clocking whole
# processes: the differences are microseconds per lookup, far below process start-up noise.
#
#   scripts/run_all_benchmarks.sh [output-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/results}"
BENCH="python3 $ROOT/scripts/bench.py"
mkdir -p "$OUT"

cd "$ROOT"
[ -x target/release/dpbst ] || cargo build --release

JOBS="$(sysctl -n hw.perflevel0.logicalcpu 2>/dev/null || getconf _NPROCESSORS_ONLN)"
echo "using $JOBS performance cores for load-independent stages"

# Suites where a solve takes long enough that process start-up is not what is being measured.
HARD_SUITES="uf250-1065:8 uuf250-1065:8 rand3:10 php parity:8 aim:15 dubois ssa bf QG:8 chain blocks grid jnh:15"
# Suites where the memo is actually exercised, per the hit rates it reports.
MEMO_SUITES="php chain ssa bf aim:15 dubois"

echo
echo "== 1. dpbst against other solvers (sequential: this one is timed) =="
$BENCH --solvers dpbst minisat varisat splr cadical kissat \
       --suites $HARD_SUITES --timeout 10 --mode time --jobs 1 \
       --out "$OUT/hard.csv" --markdown "$OUT/hard.md"

echo
echo "== 2. bucket policy: what does a tree buy over a list? (parallel, counters only) =="
$BENCH --solvers dpbst-avl dpbst-unbalanced dpbst-splay dpbst-chain \
       --suites $MEMO_SUITES --timeout 10 --mode metrics --jobs "$JOBS" \
       --out "$OUT/buckets.csv" --markdown "$OUT/buckets.md"

echo
echo "== 3. ablation: what does each technique buy? (parallel, counters only) =="
$BENCH --solvers dpbst dpbst-nocache dpbst-plain-dpll dpbst-nopure dpbst-nopre \
       --suites $MEMO_SUITES blocks grid --timeout 10 --mode metrics --jobs "$JOBS" \
       --out "$OUT/ablation.csv" --markdown "$OUT/ablation.md"

echo
echo "== 4. branching heuristics (parallel, counters only) =="
$BENCH --solvers dpbst-jw dpbst-dlis dpbst-dlcs dpbst-mom dpbst-static \
       --suites $MEMO_SUITES --timeout 10 --mode metrics --jobs "$JOBS" \
       --out "$OUT/heuristics.csv" --markdown "$OUT/heuristics.md"

echo
echo "== 5. correctness sweep against known verdicts (parallel) =="
scripts/check_verdicts.sh 300 "$JOBS"

echo
echo "== 6. cache microbenchmarks (criterion; single-threaded by design) =="
cargo bench --bench cache_bench -- --warm-up-time 1 --measurement-time 3 2>&1 | tail -40

echo
echo "summaries written to $OUT/*.md"
