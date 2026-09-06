#!/usr/bin/env bash
# Build the reference SAT solvers dpbst is compared against, into tools/bin/.
#
# Everything is built from source at a pinned release rather than installed system-wide, so the
# comparison names exact versions and needs no administrator rights.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/tools/src"
BIN="$ROOT/tools/bin"
mkdir -p "$SRC" "$BIN"

clone() { # url dir tag
  if [ ! -d "$SRC/$2" ]; then
    git clone -q --depth 1 ${3:+--branch "$3"} "$1" "$SRC/$2"
  fi
}

echo "== CaDiCaL =="
clone https://github.com/arminbiere/cadical.git cadical
( cd "$SRC/cadical" && ./configure >/dev/null && make -j"$(getconf _NPROCESSORS_ONLN)" >/dev/null )
cp "$SRC/cadical/build/cadical" "$BIN/"

echo "== Kissat =="
clone https://github.com/arminbiere/kissat.git kissat
( cd "$SRC/kissat" && ./configure >/dev/null && make -j"$(getconf _NPROCESSORS_ONLN)" >/dev/null )
cp "$SRC/kissat/build/kissat" "$BIN/"

echo "== MiniSat =="
# MiniSat 2.2 predates C++11 and does not build on a modern clang without three fixes:
#   1. a friend declaration carrying a default argument,
#   2. "%"PRIu64 string concatenation, now a reserved user-defined literal,
#   3. an out-of-line memUsedPeak whose signature lost its parameter on the Apple path,
#   4. --static, which Apple's linker does not accept.
clone https://github.com/niklasso/minisat.git minisat
(
  cd "$SRC/minisat"
  sed -i.bak 's/^inline  Lit  mkLit     (Var var, bool sign) {/inline  Lit  mkLit     (Var var, bool sign = false) {/' minisat/core/SolverTypes.h
  sed -i.bak 's/friend Lit mkLit(Var var, bool sign = false);/friend Lit mkLit(Var var, bool sign);/' minisat/core/SolverTypes.h
  find minisat \( -name '*.cc' -o -name '*.h' \) -exec sed -i.bak -E 's/"(PRI[a-zA-Z0-9_]+)/" \1/g; s/(PRI[a-zA-Z0-9_]+)"/\1 "/g' {} +
  sed -i.bak 's/^double Minisat::memUsedPeak() { return memUsed(); }/double Minisat::memUsedPeak(bool) { return memUsed(); }/; s/^double Minisat::memUsedPeak() { return 0; }/double Minisat::memUsedPeak(bool) { return 0; }/' minisat/utils/System.cc
  sed -i.bak 's/ --static//g' Makefile
  make config prefix=. >/dev/null
  make r -j"$(getconf _NPROCESSORS_ONLN)" >/dev/null
)
cp "$SRC/minisat/build/release/bin/minisat" "$BIN/"

echo "== varisat, splr (Rust) =="
cargo install --root "$ROOT/tools" --locked varisat-cli splr

echo
ls -la "$BIN"
