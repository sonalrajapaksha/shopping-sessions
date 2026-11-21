#!/bin/sh
# Runs the check on a boat.dev x86_64 sandbox (4 vCPU Ryzen, Linux) instead of the laptop.
#   bench/boat.sh time   -> last line: min wall seconds over 5 runs on the 20M-event set, NCPU=4
#   bench/boat.sh guard  -> exit 0 when 36 fixtures, dev answer, 15 oracle seeds and the frozen 20M output all match
# The sandbox id is in $BOAT_BOX or the file named by $BOAT_BOX_FILE. Datasets and oracle outputs live on the box
# (made by boatkit/setup.sh with the same generator seeds as the laptop).
set -eu
B=/Users/cheryl/.ascii/bin/boat
BOX=${BOAT_BOX:-$(cat "${BOAT_BOX_FILE:-/private/tmp/claude-501/-Users-cheryl--paradev-worktrees-builders-hackathon-organic-recess-5957a029/a1163b9f-fd93-434f-97fd-980b9bf5b471/scratchpad/boat_box}")}
$B extend "$BOX" --hours 2 >/dev/null 2>&1 || $B resume "$BOX" >/dev/null 2>&1 || true
$B push "$BOX" . --to /home/user/repo >/dev/null || { echo "push failed"; exit 1; }
case "$1" in
time) remote='
set -eu; cd /home/user/repo
RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release -q 2>&1 | tail -3
best=999
for i in 1 2 3 4 5; do
  rm -rf /tmp/o; s=$(date +%s.%N)
  DATA_DIR=/home/user/data/p20 OUTPUT_DIR=/tmp/o NCPU=4 target/release/sessions 2>/dev/null
  e=$(date +%s.%N); w=$(echo "$e - $s" | bc); echo "run $i: wall $w" >&2
  best=$(echo "if ($w < $best) $w else $best" | bc)
done
echo $best' ;;
guard) remote='
set -u; cd /home/user/repo; PY=/home/user/venv/bin/python; R=/home/user/boatkit/refs; D=/home/user/data
RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release -q 2>&1 | tail -3 || exit 1
$PY scripts/run_fixtures.py fixtures -- target/release/sessions 2>&1 | tail -1 | grep -qx "36/36 fixtures passed" || { echo "fixtures failed"; exit 1; }
check() { rm -rf /tmp/g; DATA_DIR=$1 OUTPUT_DIR=/tmp/g NCPU=4 target/release/sessions 2>/dev/null || { echo "crash on $1"; exit 1; }
  $PY scripts/compare.py /tmp/g "$2" > /tmp/cmp 2>&1 || { echo "DIFFER on $1"; cat /tmp/cmp; exit 1; }; }
check $D/dev dev-answer
for s in 2 3 4 5 6 7 8 9 10 11 12 13; do check $D/s$s $R/s$s.ref; done
for s in 21 22 23; do check $D/b$s $R/b$s.ref; done
check $D/p20 $R/p20.rs
echo "guard ok"' ;;
*) echo "usage: $0 time|guard"; exit 2 ;;
esac
$B ssh "$BOX" -- "$remote"
