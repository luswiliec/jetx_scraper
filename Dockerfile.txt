# Build stage
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim
WORKDIR /app
COPY --from=builder /app/target/release/jetx-scraper .

# Expose port (optional but good practice)
EXPOSE 3000

CMD ["./jetx-scraper"]
