# ── Stage 1: Build ────────────────────────────────────────────────
FROM rust:bookworm AS builder

WORKDIR /app
COPY . .

RUN cargo build --release

# ── Stage 2: Run (minimal image) ─────────────────────────────────
FROM debian:bookworm-slim AS runner

# Install TLS certs (needed for wss:// connections)
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/jetx_scraper /app/jetx_scraper

CMD ["/app/jetx_scraper"]
