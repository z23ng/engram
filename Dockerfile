# Multi-stage Dockerfile for Engram (`mem` + `mem-daemon`)
FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

RUN cargo build --release -p mem-cli -p mem-daemon

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/mem /usr/local/bin/mem
COPY --from=builder /app/target/release/mem-daemon /usr/local/bin/mem-daemon

ENV MEM_DB_PATH=/data/memory.db
ENV MEM_PROVIDER=disabled
ENV MEM_DAEMON_HOST=0.0.0.0
ENV MEM_DAEMON_PORT=7700

EXPOSE 7700
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/mem"]
CMD ["daemon", "--host", "0.0.0.0", "--port", "7700"]
