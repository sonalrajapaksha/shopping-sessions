# Shopping Sessions: native Rust pipeline (the DuckDB/Python version stays in pipeline/ as a reference).
# Static musl binary in a FROM scratch image: no shell, no run.sh, no libc, nothing to start but the binary.
FROM rust:1.100.0-slim-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# musl target for whatever arch this builder is (x86_64 on the scorer, aarch64 on an Apple-silicon Mac)
RUN rustup target add "$(uname -m)-unknown-linux-musl"
COPY fast/Cargo.toml fast/Cargo.lock ./
COPY fast/src ./src
RUN T="$(uname -m)-unknown-linux-musl" \
 && cargo build --release --locked --target "$T" \
 && cp "target/$T/release/shopping-fast" /shopping-fast \
 && file /shopping-fast 2>/dev/null || true

FROM scratch
COPY --from=build /shopping-fast /shopping-fast
# The scorer runs the container as uid 1000 (numeric; scratch has no passwd file). Reads DATA_DIR, OUTPUT_DIR,
# SCRATCH_DIR, NCPU from the environment itself, with defaults /data, /output, /scratch, all cores.
USER 1000:1000
ENTRYPOINT ["/shopping-fast"]
