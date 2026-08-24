# Stage 1: Builder
# Use the official Rust image to compile the project
# Pinned to match the toolchain that resolved Cargo.lock (transitive deps require edition2024 / Cargo 1.85+)
FROM rust:1.90-slim-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy dependency files first for Docker layer caching.
# This layer is only rebuilt when Cargo.toml or Cargo.lock changes.
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies in isolation
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm src/main.rs

# Copy actual source code and build the real binary
COPY src ./src
RUN touch src/main.rs
RUN cargo build --release

# Stage 2: Runtime
# Use a minimal Debian image — no Rust toolchain needed at runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user for security
RUN useradd -r -s /bin/false vela

# Create required directories
RUN mkdir -p /etc/vela /var/log/vela && \
    chown vela:vela /var/log/vela

# Copy the compiled binary from the builder stage
COPY --from=builder /build/target/release/vela /usr/local/bin/vela
RUN chmod +x /usr/local/bin/vela

# Switch to non-root user
USER vela

# Expose the API port (proxy ports are configured per-service)
EXPOSE 7700

# Use the unauthenticated /health endpoint as the Docker health probe.
# 3 retries × 10s interval = 30s before container is marked unhealthy.
HEALTHCHECK --interval=10s --timeout=5s --start-period=15s --retries=3 \
  CMD wget -qO- http://localhost:7700/health || exit 1

# Vela expects config.toml to be mounted at /etc/vela/config.toml
ENTRYPOINT ["/usr/local/bin/vela", "/etc/vela/config.toml"]
