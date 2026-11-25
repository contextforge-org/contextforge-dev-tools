FROM rust:1.100.0-trixie@sha256:e84068000361bf3d4a5ba2b7647f62893528b1eb9f847d7a23b493f5ce8ea254 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY docker ./docker
COPY scripts ./scripts
COPY benchmarks ./benchmarks
COPY tests/conformance/baselines ./tests/conformance/baselines
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release --bin cf-integration \
    && cp target/release/cf-integration /usr/local/bin/cf-integration

FROM node:26.13.1-trixie-slim@sha256:abe7e5a82b61a8ef67b315b85497c9e65173387ccf2f4a029248184b8e020be5 AS tools
ENV HOME=/tmp
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN npm install --global --ignore-scripts \
    @modelcontextprotocol/conformance@0.2.0-alpha.12 \
    @modelcontextprotocol/inspector@2.16.0 \
    && npm cache clean --force
COPY --from=build /usr/local/bin/cf-integration /usr/local/bin/cf-integration
ENTRYPOINT ["cf-integration", "__tool"]

FROM debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0 AS helpers
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/cf-integration /usr/local/bin/cf-integration
ENTRYPOINT ["cf-integration"]
