FROM rust:1.97-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY docker ./docker
COPY scripts ./scripts
COPY tests/conformance/baselines ./tests/conformance/baselines
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release --bin cf-integration \
    && cp target/release/cf-integration /usr/local/bin/cf-integration

FROM node:22-bookworm-slim AS tools
ENV HOME=/tmp
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN npm install --global --ignore-scripts \
    @modelcontextprotocol/conformance@0.2.0-alpha.11 \
    @modelcontextprotocol/inspector@2.2.0 \
    && npm cache clean --force
COPY --from=build /usr/local/bin/cf-integration /usr/local/bin/cf-integration
ENTRYPOINT ["cf-integration", "__tool"]

FROM debian:bookworm-slim AS helpers
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/cf-integration /usr/local/bin/cf-integration
ENTRYPOINT ["cf-integration"]
