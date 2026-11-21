#!/bin/sh
# Correctness gate: 36 fixtures, dev answer, 15 seeds vs the Python oracle, 20M vs frozen baseline output.
set -u
G=/private/tmp/claude-501/-Users-cheryl--paradev-worktrees-builders-hackathon-organic-recess-5957a029/a1163b9f-fd93-434f-97fd-980b9bf5b471/scratchpad
PY=.venv/bin/python
cargo build --release -q || exit 1
$PY scripts/run_fixtures.py fixtures -- target/release/sessions 2>&1 | tail -1 | grep -qx "36/36 fixtures passed" || { echo "fixtures failed"; exit 1; }
O=$(mktemp -d "$G/guard.XXXX"); trap 'rm -rf "$O"' EXIT
check() { # data expected
  rm -rf "$O/o"; DATA_DIR=$1 OUTPUT_DIR=$O/o NCPU=8 target/release/sessions 2>/dev/null || { echo "crash on $1"; exit 1; }
  $PY scripts/compare.py "$O/o" "$2" > "$O/cmp" 2>&1 || { echo "DIFFER on $1"; cat "$O/cmp"; exit 1; }
}
check $G/data/dev dev-answer
for s in 2 3 4 5 6 7 8 9 10 11 12 13; do check $G/golden/s$s $G/golden/s$s.ref; done
for s in 21 22 23; do check $G/golden/b$s $G/golden/b$s.ref; done
check $G/data/p20 $G/golden/p20.rs
echo "guard ok"
