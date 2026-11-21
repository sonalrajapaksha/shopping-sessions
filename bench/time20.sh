#!/bin/sh
# Prints the minimum wall seconds over 5 runs of the release binary on the 20M-event set, 4 threads (last line = score).
# 4 threads: the scorer has 4 physical cores (8 hyperthreads), so CPU savings show in its wall time; with 8 threads on
# this 15-core laptop they mostly do not.
# Wall is what the scorer times. Minimum, not mean: the dev laptop is shared (load avg 20-30) and contention only
# ever adds time, so the minimum is the steady estimate (min-of-5 drifts ~2-4%; single runs drift 10-100%).
set -eu
G=/private/tmp/claude-501/-Users-cheryl--paradev-worktrees-builders-hackathon-organic-recess-5957a029/a1163b9f-fd93-434f-97fd-980b9bf5b471/scratchpad
cargo build --release -q
best=999999
for i in 1 2 3 4 5; do
  O=$(mktemp -d "$G/bench.XXXX")
  R=$(DATA_DIR=$G/data/p20 OUTPUT_DIR=$O NCPU=4 /usr/bin/time -p target/release/sessions 2>&1 >/dev/null)
  rm -rf "$O"
  w=$(echo "$R" | awk '/^real/{print $2}')
  echo "run $i: wall $w $(echo "$R" | awk '/^user/{u=$2} /^sys/{s=$2} END {printf "cpu %.2f", u+s}')" >&2
  best=$(python3 -c "print(min($best, $w))")
done
echo "$best"
