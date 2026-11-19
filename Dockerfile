# Multi-stage Dockerfile for AI Memory (`mem`)
FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

RUN cargo build --release -p mem-cli

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/mem /usr/local/bin/mem

ENV MEM_DB_PATH=/data/memory.db
ENV MEM_PROVIDER=disabled

VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/mem"]
CMD ["doctor"]
