# syntax=docker/dockerfile:1.7
#
# Multi-stage: cargo-chef caches the dependency build (aws-lc-rs and a fat-LTO
# hyper stack are the slow part) so only `src/` changes rebuild. The runtime
# stage is distroless — glibc, no shell, no package manager.
#
# The build is a two-pass PGO: an instrumented binary is built and trained
# against the relay's own test suite plus a live serve loop, and the resulting
# profile drives the shipped build. `.cargo/config.toml` is copied into both
# passes so the nightly `-Z` flags have exactly one definition; each pass adds
# only its own `-Cprofile-*` flag on top.

FROM rust:1.95-bookworm AS chef
# aws-lc-rs compiles its C core with cmake; perl generates the assembly.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake perl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
# The tuned build is nightly-only: build-std plus the -Z size flags. The pin and
# its components live in rust-toolchain.toml, so the image compiles with exactly
# the compiler the host does; `rustup show` is what materialises it here.
# rust-src is what build-std compiles, and llvm-tools carries the llvm-profdata
# that has to match this rustc's LLVM rather than the distro's.
COPY rust-toolchain.toml ./
RUN rustup show
RUN cargo install cargo-chef --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------- PGO pass 1
# Instrumented build + training run. LTO is off and codegen-units raised here
# purely for speed: this binary is thrown away, and profile counters are keyed
# on the pre-LTO CFG, so the profile stays valid for the fat-LTO build below.
FROM chef AS pgo
RUN apt-get update \
 && apt-get install -y --no-install-recommends apache2-utils curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN sed -i 's#^rustflags = \[#rustflags = ["-Cprofile-generate=/pgo/raw", #' .cargo/config.toml \
 && grep rustflags .cargo/config.toml
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    cargo build --release --locked --bin llm-relay
# Training, in two halves. The test suite is what covers the SSE frame scanner,
# the retry and the error envelopes — a /healthz flood never reaches any of it.
# The serve loop then covers accept, h1 and h2 parsing, routing and the 404/405
# arms, which the tests in turn never reach.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    cargo test --release --locked
RUN set -eu; \
    PORT=18787 LLVM_PROFILE_FILE=/pgo/raw/serve_%p.profraw \
      ./target/release/llm-relay & \
    pid=$!; \
    for _ in $(seq 1 400); do curl -sf -o /dev/null http://127.0.0.1:18787/healthz && break; sleep 0.05; done; \
    ab -c 50 -n 20000 -k http://127.0.0.1:18787/healthz > /dev/null 2>&1 || true; \
    ab -c 20 -n 2000     http://127.0.0.1:18787/ > /dev/null 2>&1 || true; \
    curl -sf -o /dev/null http://127.0.0.1:18787/egress || true; \
    curl -s  -o /dev/null -X GET http://127.0.0.1:18787/v1/chat/completions || true; \
    curl -s  -o /dev/null http://127.0.0.1:18787/nope || true; \
    kill -TERM "$pid"; \
    for _ in $(seq 1 300); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done; \
    profdata=$(find "$(rustc --print sysroot)/lib/rustlib" -name llvm-profdata | head -1); \
    "$profdata" merge -o /pgo/merged.profdata /pgo/raw/*.profraw; \
    ls -l /pgo/merged.profdata

# ---------------------------------------------------------------- PGO pass 2
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
COPY .cargo ./.cargo
COPY --from=pgo /pgo/merged.profdata /pgo/merged.profdata
# `-Cprofile-use` on a function the profile never saw is not an error, only a
# missing hint, so the flood of warnings from cold code is silenced.
RUN sed -i 's#^rustflags = \[#rustflags = ["-Cprofile-use=/pgo/merged.profdata", "-Cllvm-args=-pgo-warn-missing-function=false", #' .cargo/config.toml \
 && grep rustflags .cargo/config.toml
# The dependency layer: reused until Cargo.lock or the profile changes.
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
