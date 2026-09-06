#!/usr/bin/env bash
# Download the SATLIB benchmark suites this project is measured on.
#
# Instances are not committed: there are 7,586 of them. This script fetches, extracts, flattens
# and normalises them into benchmarks/clean/, which is what scripts/bench.py reads.
#
#   scripts/fetch_benchmarks.sh          # everything (~35 MB download)
#   scripts/fetch_benchmarks.sh uf20-91  # one suite
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE="https://www.cs.ubc.ca/~hoos/SATLIB/Benchmarks/SAT"
DL="$ROOT/benchmarks/_dl"
SUITES="$ROOT/benchmarks/suites"
CLEAN="$ROOT/benchmarks/clean"

# suite name -> path under $BASE
declare -a ARCHIVES=(
  "uf20-91:RND3SAT/uf20-91"
  "uf50-218:RND3SAT/uf50-218"
  "uuf50-218:RND3SAT/uuf50-218"
  "uf75-325:RND3SAT/uf75-325"
  "uuf75-325:RND3SAT/uuf75-325"
  "uf100-430:RND3SAT/uf100-430"
  "uuf100-430:RND3SAT/uuf100-430"
  "flat30-60:GCP/flat30-60"
  "flat50-115:GCP/flat50-115"
  "flat75-180:GCP/flat75-180"
  "flat100-239:GCP/flat100-239"
  "ais:AIS/ais"
  "blocksworld:PLANNING/BlocksWorld/blocksworld"
  "logistics:PLANNING/Logistics/logistics"
  "aim:DIMACS/AIM/aim"
  "CBS_k3_n100_m403_b10:CBS/CBS_k3_n100_m403_b10"
)

wanted=("$@")
mkdir -p "$DL" "$SUITES"

for entry in "${ARCHIVES[@]}"; do
  name="${entry%%:*}"
  path="${entry#*:}"
  if [ ${#wanted[@]} -gt 0 ] && ! printf '%s\n' "${wanted[@]}" | grep -qx "$name"; then
    continue
  fi

  archive="$DL/$name.tar.gz"
  if [ ! -s "$archive" ]; then
    echo "fetching $name"
    curl -sSfL -o "$archive" "$BASE/$path.tar.gz" || { echo "  failed: $name" >&2; rm -f "$archive"; continue; }
  fi

  mkdir -p "$SUITES/$name"
  tar xzf "$archive" -C "$SUITES/$name"
  # Some archives nest the instances several directories deep; flatten them.
  find "$SUITES/$name" -mindepth 2 -name '*.cnf' -exec mv {} "$SUITES/$name/" \;
  find "$SUITES/$name" -mindepth 1 -type d -empty -delete
  printf '  %-24s %s instances\n' "$name" "$(find "$SUITES/$name" -name '*.cnf' | wc -l | tr -d ' ')"
done

# Strip SATLIB's trailing '%' marker, which MiniSat, CaDiCaL and Kissat all refuse to parse.
python3 "$ROOT/scripts/normalize_cnf.py" "$SUITES" "$CLEAN"
