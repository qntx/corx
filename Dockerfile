# syntax=docker/dockerfile:1.9
#
# Multi-stage build for the `corx` link-preview server.
#
# * Stage 1 (`chef`)    -- pin cargo-chef, the dependency-graph caching tool.
# * Stage 2 (`planner`) -- emit recipe.json describing the dep graph only.
# * Stage 3 (`builder`) -- cook deps from the recipe, then compile the bin.
# * Stage 4 (`runtime`) -- distroless runtime image, non-root.
#
# Multi-arch: the base images are multi-arch out of the box.

ARG RUST_VERSION=1.94

FROM rust:${RUST_VERSION}-slim-bookworm AS chef
ENV CARGO_TERM_COLOR=always
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pkg-config build-essential ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked --version ^0.1
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
# Cook deps once; rebuilds reuse this layer until the dep graph changes.
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release --locked -p corx-server && strip target/release/corx

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
LABEL org.opencontainers.image.title="corx" \
      org.opencontainers.image.description="Link-preview metadata service" \
      org.opencontainers.image.source="https://github.com/qntx/corx" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

COPY --from=builder /src/target/release/corx /corx

ENV RUST_LOG=info

EXPOSE 8080
USER nonroot:nonroot

ENTRYPOINT ["/corx"]
