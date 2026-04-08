# Build stage
FROM rust:latest as builder

WORKDIR /app

# Cache dependencies first (faster builds)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main(){}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

# Copy real source
COPY . .
RUN cargo build --release

# Runtime stage (small image)
FROM debian:bookworm-slim

WORKDIR /app

# Install SSL certs (needed for WebSockets!)
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/jetx_scraper .

EXPOSE 8000

CMD ["./jetx_scraper"]
