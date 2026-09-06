#!/usr/bin/env bash
# Check dpbst against suites whose verdicts are known from their names.
#
# SATLIB names satisfiable random 3-SAT suites `uf*` and unsatisfiable ones `uuf*`, which gives a
# few thousand instances of free ground truth. Satisfiable answers are additionally re-checked
# against the input with --verify, so a wrong model fails as loudly as a wrong verdict.
#
# Only verdicts are checked here, never timings, so this runs in parallel. Concurrency defaults
# to the performance-core count: on Apple silicon the surplus would otherwise land on efficiency
# cores, which costs wall time without buying throughput.
#
#   scripts/check_verdicts.sh [max-instances-per-suite] [jobs]
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DPBST="$ROOT/target/release/dpbst"
CLEAN="$ROOT/benchmarks/clean"
LIMIT="${1:-200}"
JOBS="${2:-$(sysctl -n hw.perflevel0.logicalcpu 2>/dev/null || getconf _NPROCESSORS_ONLN)}"
TIMEOUT_SECS="${DPBST_TIMEOUT:-30}"

[ -x "$DPBST" ] || { echo "build it first: cargo build --release" >&2; exit 2; }

# Checks one instance. Prints a line only on failure, so silence is success.
check_one() {
  local f="$1" expected="$2" code=0
  if [ "$expected" -eq 10 ]; then
    "$DPBST" "$f" --no-model --verify -t "$TIMEOUT_SECS" >/dev/null 2>&1 || code=$?
  else
    "$DPBST" "$f" --no-model -t "$TIMEOUT_SECS" >/dev/null 2>&1 || code=$?
  fi
  [ "$code" -eq "$expected" ] || echo "FAIL $f: got exit $code, expected $expected"
}
export -f check_one
export DPBST TIMEOUT_SECS

failures=$(mktemp)
checked=0

for spec in "uf20-91:10" "uf50-218:10" "uf75-325:10" "uf100-430:10" \
            "uuf50-218:20" "uuf75-325:20" "uuf100-430:20"; do
  suite="${spec%%:*}"
  expected="${spec##*:}"
  dir="$CLEAN/$suite"
  [ -d "$dir" ] || { echo "skipping $suite (not fetched)"; continue; }

  mapfile -t files < <(ls "$dir"/*.cnf 2>/dev/null | head -"$LIMIT")
  checked=$((checked + ${#files[@]}))
  printf '  %-16s %4d instances, %s jobs\n' "$suite" "${#files[@]}" "$JOBS"
  printf '%s\n' "${files[@]}" \
    | xargs -P "$JOBS" -I{} bash -c 'check_one "$@"' _ {} "$expected" >>"$failures"
done

echo
if [ -s "$failures" ]; then
  cat "$failures" >&2
  echo "verdict mismatches found" >&2
  rm -f "$failures"
  exit 1
fi
rm -f "$failures"
echo "all $checked instances matched their expected verdict"
