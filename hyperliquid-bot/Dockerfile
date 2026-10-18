FROM rust:slim-bookworm AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    CARGO_PROFILE_RELEASE_STRIP=symbols
COPY Cargo.toml Cargo.lock ./
# Pre-build dummy source to cache heavy dependency crates (ethers, SDK, ndarray)
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release
COPY src/ ./src/
# Touch main to force rebuild of project binary
RUN touch src/main.rs && cargo build --release

# Stage 2: Minimal Runtime (~20MB)
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/hyperliquid-bot /usr/local/bin/

CMD ["hyperliquid-bot"]
