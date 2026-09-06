#!/usr/bin/env bash
# Regenerate the paper's figures and tables from results/*.csv, then typeset it.
#
# Run scripts/run_all_benchmarks.sh first if the CSVs are missing or stale.
#
#   scripts/build_paper.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "== figures and tables =="
uv run --script scripts/academic_plots.py

echo "== typesetting =="
( cd paper && latexmk -pdf -interaction=nonstopmode -halt-on-error paper.tex )

echo "wrote paper/paper.pdf"
