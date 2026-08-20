# syntax=docker/dockerfile:1.7
#
# Multi-stage: cargo-chef caches the dependency build (aws-lc-rs and a fat-LTO
# hyper stack are the slow part) so only `src/` changes rebuild. The runtime
# stage is distroless — glibc, no shell, no package manager.

FROM rust:1.95-bookworm AS chef
# aws-lc-rs compiles its C core with cmake; perl generates the assembly.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake perl \
 && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# The dependency layer: reused until Cargo.lock changes.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --locked --bin llm-relay

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /app/target/release/llm-relay /llm-relay
# The only variable the relay reads: platforms assign the port they route to.
ENV PORT=8787
EXPOSE 8787
USER nonroot
# There is no shell and no curl in this image, so the probe is the binary
# calling its own /healthz over the loopback.
# 60s rather than 30s: the probe is a process spawn, and on a platform that
# bills CPU by the minute there is no reason to pay for it twice as often.
HEALTHCHECK --interval=60s --timeout=5s --start-period=2s --retries=3 \
    CMD ["/llm-relay", "--healthcheck"]
ENTRYPOINT ["/llm-relay"]
