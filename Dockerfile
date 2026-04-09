# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1 AS builder

WORKDIR /app

# Copy manifests first so dependency layer is cached between rebuilds
COPY Cargo.toml ./
# Create a dummy main so cargo can fetch + compile deps without your real code
RUN mkdir src && echo 'fn main(){}' > src/main.rs
RUN cargo build --release
RUN rm src/main.rs

# Now copy the real source and do the final build
COPY src ./src
# Touch main.rs so cargo knows it changed
RUN touch src/main.rs
RUN cargo build --release

# ── Stage 2: run ──────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# Install OpenSSL runtime (needed by native-tls at runtime)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/jetx_scraper .

EXPOSE 8000

CMD ["./jetx_scraper"]
