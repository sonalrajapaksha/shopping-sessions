# Shopping Sessions: native Rust pipeline (the DuckDB/Python version stays in pipeline/ as a reference).
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY fast/Cargo.toml fast/Cargo.lock ./
COPY fast/src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
# The scorer runs the container as uid 1000; give that uid a name and a home.
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app
COPY --from=build /src/target/release/shopping-fast /app/shopping-fast
COPY run.sh /app/run.sh
RUN chmod +x /app/run.sh /app/shopping-fast
WORKDIR /app
CMD ["/app/run.sh"]
