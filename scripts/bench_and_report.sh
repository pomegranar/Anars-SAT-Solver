#!/usr/bin/env bash
# Run the whole benchmark suite, typeset a PDF report, and Taildrop it to a phone.
#
# Intended to be started in tmux and left alone:
#
#     tmux new -s bench 'scripts/bench_and_report.sh'
#
# Only the cross-solver timing stage runs sequentially; see scripts/run_all_benchmarks.sh. Expect
# roughly 30-40 minutes on an M1.
#
#   scripts/bench_and_report.sh [tailscale-device] [output-dir]
#
# The report is still built and sent if a benchmark stage fails, with the failure recorded, so a
# long unattended run never ends with nothing to show for itself.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEVICE="${1:-iphone16anar}"
OUT="${2:-$ROOT/results}"
PDF="$OUT/report.pdf"
LOG="$OUT/all.log"

cd "$ROOT"
mkdir -p "$OUT"

# Tailscale ships a CLI in the app bundle when it was not installed via Homebrew.
TAILSCALE="$(command -v tailscale || true)"
[ -n "$TAILSCALE" ] || TAILSCALE="/Applications/Tailscale.app/Contents/MacOS/Tailscale"

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "checking prerequisites"
missing=0
for tool in cargo uv latexmk; do
  command -v "$tool" >/dev/null || { echo "missing: $tool"; missing=1; }
done
[ -x "$TAILSCALE" ] || { echo "missing: tailscale CLI"; missing=1; }
[ "$missing" -eq 0 ] || { echo "install the tools above and re-run" >&2; exit 2; }
echo "ok"

say "building the solver"
cargo build --release || exit 1

started=$(date +%s)
say "running benchmarks (logging to $LOG)"
# Deliberately not fatal: a failed stage should still produce a report saying so.
bash scripts/run_all_benchmarks.sh "$OUT" 2>&1 | tee "$LOG"
bench_status=${PIPESTATUS[0]}
elapsed=$(( $(date +%s) - started ))
[ "$bench_status" -eq 0 ] \
  && echo "benchmarks finished in ${elapsed}s" \
  || echo "WARNING: benchmark stage exited $bench_status after ${elapsed}s; reporting anyway"

say "building the PDF report"
# uv resolves matplotlib and pandas from the inline metadata in report.py into a scratch
# environment, so nothing is installed into the system Python.
uv run --script scripts/report.py --out "$PDF"
report_status=$?

if [ "$report_status" -ne 0 ] || [ ! -s "$PDF" ]; then
  echo "report generation failed; sending the raw log instead" >&2
  PDF="$LOG"
fi

say "sending $(basename "$PDF") to $DEVICE"
if "$TAILSCALE" file cp "$PDF" "${DEVICE}:"; then
  echo "sent to $DEVICE — accept the file on the device"
else
  echo "Taildrop failed. Check '$TAILSCALE status' and that $DEVICE is online." >&2
  echo "The report is still at $PDF" >&2
  exit 1
fi

say "done"
printf 'benchmarks: %s\nreport:     %s\nsummaries:  %s\n' \
  "$([ "$bench_status" -eq 0 ] && echo ok || echo "FAILED ($bench_status)")" \
  "$PDF" "$OUT/*.md"
