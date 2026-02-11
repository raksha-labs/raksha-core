# Multi-stage build for Rust DeFi Surveillance services
# Builds: indexer, detector, scorer, orchestrator, finality

FROM rust:1.75-slim as builder

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

# Build release binaries
RUN cargo build --release \
    -p indexer \
    -p detector \
    -p scorer \
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

# Copy binaries from builder
COPY --from=builder /app/target/release/indexer /app/bin/indexer
COPY --from=builder /app/target/release/detector /app/bin/detector
COPY --from=builder /app/target/release/scorer /app/bin/scorer
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

# Health check endpoint (assuming workers expose metrics on 9090)
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD [ "sh", "-c", "nc -z localhost 9090 || exit 1" ]

# Use ARG to determine which binary to run
ARG WORKER=indexer
ENV WORKER_BINARY=${WORKER}

CMD ["/app/bin/${WORKER_BINARY}"]
