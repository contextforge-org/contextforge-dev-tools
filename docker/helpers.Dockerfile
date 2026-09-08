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

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/cf-integration /usr/local/bin/cf-integration
ENTRYPOINT ["cf-integration"]
