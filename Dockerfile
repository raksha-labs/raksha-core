# Multi-stage build for Rust DeFi Surveillance services.
# Local Docker stack binaries: indexer + detector.

FROM rust:slim AS builder

# Install system dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy workspace manifests
COPY Cargo.toml Cargo.lock ./

# Copy all crate sources and apps
COPY crates ./crates
COPY apps ./apps

# Build release binaries required for core runtimes.
RUN cargo build --release \
    -p indexer \
    -p detector \
    -p orchestrator \
    -p finality

# Runtime stage - minimal image
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1001 -s /bin/bash appuser

WORKDIR /app

# Copy binaries from builder.
COPY --from=builder /app/target/release/indexer /app/bin/indexer
COPY --from=builder /app/target/release/detector /app/bin/detector
COPY --from=builder /app/target/release/orchestrator /app/bin/orchestrator
COPY --from=builder /app/target/release/finality /app/bin/finality

# Copy schemas and rules (needed for runtime validation)
COPY schemas ./schemas
COPY rules ./rules

# Set ownership
RUN chown -R appuser:appuser /app

USER appuser

# Default to ingestion worker (override with CMD)
ENV RUST_LOG=info
ENV RUST_BACKTRACE=1

# Default command can be overridden in docker-compose.
CMD ["/app/bin/indexer"]
