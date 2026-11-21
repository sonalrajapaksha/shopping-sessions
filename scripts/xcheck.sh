#!/bin/sh
# usage: xcheck.sh SEED EVENTS  -- compares native Rust output with the DuckDB reference pipeline
set -eu
S=$1; E=$2; C=$((E/4))
D=/tmp/xc/data$S; rm -rf $D /tmp/xc/r$S /tmp/xc/d$S; mkdir -p $D /tmp/xc/r$S /tmp/xc/d$S /tmp/xc/scr$S
tools/generator/generator/target/release/generate --seed $S --events $E --clicks $C --output $D >/dev/null
DATA_DIR=$D OUTPUT_DIR=/tmp/xc/r$S fast/target/release/shopping-fast 2>/dev/null
SCRATCH_DIR=/tmp/xc/scr$S DATA_DIR=$D OUTPUT_DIR=/tmp/xc/d$S .venv/bin/python pipeline/pipeline.py >/dev/null 2>&1
echo seed $S: $(.venv/bin/python scripts/compare.py /tmp/xc/r$S /tmp/xc/d$S | tail -1)
