#!/usr/bin/env bash
# Check dpbst against suites whose verdicts are known from their names.
#
# SATLIB names satisfiable random 3-SAT suites `uf*` and unsatisfiable ones `uuf*`, which gives a
# few thousand instances of free ground truth. Satisfiable answers are additionally re-checked
# against the input with --verify, so a wrong model fails as loudly as a wrong verdict.
#
#   scripts/check_verdicts.sh [max-instances-per-suite]
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DPBST="$ROOT/target/release/dpbst"
CLEAN="$ROOT/benchmarks/clean"
LIMIT="${1:-200}"
TIMEOUT_SECS="${DPBST_TIMEOUT:-30}"

[ -x "$DPBST" ] || { echo "build it first: cargo build --release" >&2; exit 2; }

fail=0
checked=0

check_suite() { # suite expected-exit-code
  local suite="$1" expected="$2" dir="$CLEAN/$1"
  [ -d "$dir" ] || { echo "skipping $suite (not fetched)"; return; }

  local n=0
  for f in "$dir"/*.cnf; do
    [ "$n" -ge "$LIMIT" ] && break
    n=$((n + 1))
    checked=$((checked + 1))

    local code=0
    if [ "$expected" -eq 10 ]; then
      "$DPBST" "$f" --no-model --verify -t "$TIMEOUT_SECS" >/dev/null 2>&1 || code=$?
    else
      "$DPBST" "$f" --no-model -t "$TIMEOUT_SECS" >/dev/null 2>&1 || code=$?
    fi

    if [ "$code" -ne "$expected" ]; then
      echo "FAIL $f: got exit $code, expected $expected"
      fail=1
    fi
  done
  printf '  %-16s %4d instances checked\n' "$suite" "$n"
}

echo "checking known-SAT suites"
for s in uf20-91 uf50-218 uf75-325 uf100-430; do check_suite "$s" 10; done

echo "checking known-UNSAT suites"
for s in uuf50-218 uuf75-325 uuf100-430; do check_suite "$s" 20; done

echo
if [ "$fail" -eq 0 ]; then
  echo "all $checked instances matched their expected verdict"
else
  echo "verdict mismatches found" >&2
fi
exit "$fail"
