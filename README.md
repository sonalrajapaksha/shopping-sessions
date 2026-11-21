# Builders Buildathon: Shopping Sessions

This is the official DuckDB starter with local development and benchmarking tools. The challenge is to process shopping events into exact `user_daily` and `campaign_daily` outputs. The fastest **exactly correct** solution wins; consult the [official challenge](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions) for current scoring and submission details. [rules.md](rules.md) is authoritative for the data specification.

**Implementation status:** the starter writes correctly shaped but empty tables. It is not a solution and correctness checks are expected to fail until `pipeline/pipeline.py` implements the rules.

## Layout and prerequisites

`pipeline/` contains the starter, `fixtures/` the 36 official cases, `scripts/` the official test and Docker runners, `tools/generator/generator/` the official Rust generator, and `benchmarks/` the local benchmark runner. Generated data, outputs, reference answers and build artifacts are ignored by Git. The root Dockerfile is the official starter Dockerfile.

Install Git, Make, Python 3.10+, Rust/Cargo 1.85+, and Docker with Buildx and a running Linux engine. GitHub CLI is useful for submission. Run `make setup` to create a local virtual environment with the checker dependencies, then `make build` to build the scorer's `linux/amd64` image. Apple Silicon uses emulation for AMD64; its timings do not predict scorer performance. For quick native iteration, `make build PLATFORM=linux/arm64` is available.

## Generate and check

```sh
make generate-small                  # 10,000 events, 2,500 clicks
make generate-medium                 # 100,000 events
make generate-dev                    # official seed 1, 1M events, 250K clicks
make generate-performance            # 5M events
make generate-large                  # 20M events; requires substantial disk space
make generate-small SEED=42          # reproducible alternative seed
make test-fixtures                   # builds image and checks all 36 fixtures
make fetch-answer                    # download and verify the official reference archive
make run-dev                         # builds and runs dev/ into out/
make check-dev                       # exact comparison to data/dev-answer/
```

Dataset targets default to seed 1; `SEED` overrides it, but official reference answers apply only to the public dev dataset at seed 1. Each size uses one click per four events. The generator itself accepts custom counts: `tools/generator/generator/target/release/generate --seed 1 --events 10000 --clicks 2500 --output data/custom` (minimum 1,000 events). `make dev`, `make fixtures`, and the other original starter targets remain available. The official compare script reports rows only on each side and duplicate keys, including examples; differing rows appear on both sides. No approximate comparisons are used. To run fixtures without Docker: `.venv/bin/python scripts/run_fixtures.py fixtures -- .venv/bin/python pipeline/pipeline.py`.

## Benchmark

```sh
make benchmark DATASET=data/small SEED=1 REPEATS=3 TIMEOUT=3600
```

Build first. Results are JSON under `benchmarks/results/`; elapsed time covers Docker startup through container exit, excludes build, and the median includes successful runs only. Exit status, timeout and Docker OOM state are recorded. `SEED` labels the run; it does not inspect or regenerate the dataset. Run correctness checks separately. The runner uses AMD64, 8 CPUs, 24 GB with no swap, no network, read-only root and data, 64 MB `/tmp`, UID 1000, writable output and scratch, and the scorer environment variables. Bind mounts do **not** enforce the scorer's 50 GB combined output/scratch limit, and peak memory is currently not collected (JSON reports null). Ensure Docker has sufficient CPU and memory allocated. On macOS, bind-mount performance and AMD64 emulation make results non-comparable to native scorer timings.

## Submission and constraints

The scorer runs the Docker image without arguments, with `DATA_DIR=/data`, `OUTPUT_DIR=/output`, `SCRATCH_DIR=/scratch`, `HOME=/scratch`, `NCPU=8`, and `MEM_GB=24`. It expects Parquet folders for both tables, exact columns and types, no network access, and a read-only root. The combined output and scratch limit is 50 GB; the repository must stay under 50 MB. The starter configures DuckDB spilling in scratch. CSV edge cases, timestamp parsing, schema types, duplicate keys, and unexpected nulls can cause exact mismatches; follow the rules rather than inferring behavior from DuckDB defaults.

Push a public repository and submit its URL on the challenge page. Enter the desired `git rev-parse HEAD` commit SHA to score that revision, or leave it blank to use the default branch's latest commit. Do not commit generated data or answer archives. `make clean` removes generated datasets, output, benchmark results and generator build artifacts; it retains downloaded reference answers.
