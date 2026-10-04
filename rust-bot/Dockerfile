FROM rust:slim-bookworm AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Create dummy source to cache dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release
COPY src/ ./src/
# Touch main to force rebuild
RUN touch src/main.rs && cargo build --release

# Stage 2: Runtime (~15MB)
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/trading-bot /usr/local/bin/

# Execute binary
CMD ["trading-bot"]
