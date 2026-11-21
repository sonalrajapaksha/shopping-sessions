#!/bin/sh
# Cross-check the Rust pipeline against ref/reference.py on generated datasets.
# Usage: scripts/crosscheck.sh WORKDIR EVENTS SEED...
set -u
WORK=$1; EVENTS=$2; shift 2
GEN=${GEN:-generate}
fail=0
for seed in "$@"; do
  d=$WORK/s$seed-$EVENTS
  [ -d "$d" ] || "$GEN" --seed "$seed" --events "$EVENTS" --clicks $((EVENTS / 4)) --output "$d" >/dev/null || { echo "seed $seed: generate failed"; fail=1; continue; }
  rm -rf "$d.rs" "$d.ref"
  DATA_DIR=$d OUTPUT_DIR=$d.rs target/release/sessions 2>/dev/null || { echo "seed $seed: sessions exit $?"; fail=1; continue; }
  DATA_DIR=$d OUTPUT_DIR=$d.ref .venv/bin/python ref/reference.py >/dev/null 2>&1 || { echo "seed $seed: reference exit $?"; fail=1; continue; }
  if .venv/bin/python scripts/compare.py "$d.rs" "$d.ref" > "$d.cmp" 2>&1; then echo "seed $seed: MATCH"; else echo "seed $seed: DIFFER"; cat "$d.cmp"; fail=1; fi
done
exit $fail
