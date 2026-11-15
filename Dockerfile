FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY adapters ./adapters
COPY apps/http-api ./apps/http-api
ARG CARGO_BUILD_JOBS=1
RUN --mount=type=cache,id=ekyc-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=ekyc-cargo-target,target=/src/target \
    cargo build --jobs "$CARGO_BUILD_JOBS" --locked --release --bin ekyc-api --bin ekyc-worker --bin ekyc-webhook && \
    install -D -t /out target/release/ekyc-api target/release/ekyc-worker target/release/ekyc-webhook

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /usr/local/bin/
COPY config/provider-contracts.json /config/provider-contracts.json
USER 65534:65534
CMD ["ekyc-api"]
