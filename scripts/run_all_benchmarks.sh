#!/usr/bin/env bash
# Reproduce every measurement quoted in the README, in order.
#
# Run on an otherwise idle machine: scripts/bench.py times solvers sequentially, and anything
# else competing for the CPU shows up directly in the numbers.
#
#   scripts/run_all_benchmarks.sh [output-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/results}"
BENCH="python3 $ROOT/scripts/bench.py"
mkdir -p "$OUT"

cd "$ROOT"
[ -x target/release/dpbst ] || cargo build --release

# The suites where timing means something: everything here takes long enough that process
# start-up is not what is being measured.
HARD_SUITES="uf250-1065:8 uuf250-1065:8 rand3:10 php parity:8 aim:15 dubois pret ssa bf QG:8 chain blocks grid jnh:15"

# Where the memo is actually exercised, per the hit rates in the README.
MEMO_SUITES="php chain ssa bf aim:15 dubois pret"

echo "== 1. dpbst against other solvers =="
$BENCH --solvers dpbst minisat varisat splr cadical kissat \
       --suites $HARD_SUITES --timeout 10 \
       --out "$OUT/hard.csv" --markdown "$OUT/hard.md"

echo "== 2. bucket policy: does a tree beat a linked list? =="
$BENCH --solvers dpbst-avl dpbst-unbalanced dpbst-splay dpbst-chain \
       --suites $MEMO_SUITES --timeout 10 \
       --out "$OUT/buckets.csv" --markdown "$OUT/buckets.md"

echo "== 3. ablation: what does each technique buy? =="
$BENCH --solvers dpbst dpbst-nocache dpbst-plain-dpll dpbst-nopure dpbst-nopre \
       --suites $MEMO_SUITES blocks grid --timeout 10 \
       --out "$OUT/ablation.csv" --markdown "$OUT/ablation.md"

echo "== 4. branching heuristics =="
$BENCH --solvers dpbst-jw dpbst-dlis dpbst-dlcs dpbst-mom dpbst-static \
       --suites $MEMO_SUITES --timeout 10 \
       --out "$OUT/heuristics.csv" --markdown "$OUT/heuristics.md"

echo "== 5. Davis-Putnam (1960) against DPLL =="
$BENCH --solvers dpbst dpbst-davis-putnam \
       --suites php:5 dubois:6 aim:10 jnh:10 --timeout 10 \
       --out "$OUT/algorithms.csv" --markdown "$OUT/algorithms.md"

echo "== 6. correctness sweep against known verdicts =="
scripts/check_verdicts.sh 300

echo "== 7. cache microbenchmarks (criterion) =="
cargo bench --bench cache_bench

echo
echo "summaries written to $OUT/*.md"
