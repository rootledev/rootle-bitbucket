# syntax=docker/dockerfile:1
# Multi-stage musl static build, mirroring rootle's layout.

FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev \
    && rustup component add clippy rustfmt
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

FROM builder AS test
RUN cargo fmt --check \
    && cargo clippy --locked --all-targets -- -D warnings \
    && cargo test --locked

# Stripped static release binary.
FROM builder AS release
RUN cargo build --release --locked \
    && strip target/release/rootle-bitbucket \
    && ldd target/release/rootle-bitbucket 2>&1 | grep -q "Not a valid dynamic program\|not a dynamic executable" \
    && echo "static: ok"
