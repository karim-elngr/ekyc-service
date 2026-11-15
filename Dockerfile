FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release --bin ekyc-api --bin ekyc-worker --bin ekyc-webhook

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/ekyc-api /src/target/release/ekyc-worker /src/target/release/ekyc-webhook /usr/local/bin/
USER 65534:65534
CMD ["ekyc-api"]
