# Build stage
FROM rust:1.97-alpine AS builder

# Add dependencies for building
RUN apk add --no-cache musl-dev llvm-dev clang pkgconfig openssl-dev

WORKDIR /build

# Create dummy project for caching dependencies
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && : > src/lib.rs && cargo build --release && rm -rf src

# Copy source code
COPY src ./src

# Build the real binary.
# The cache layer built the same package with a stub main.rs and an empty
# lib.rs, and COPY keeps one timestamp for the whole directory, so touching a
# single file is not enough: cargo then re-uses the stub library and the binary
# cannot find its own modules. Touch everything the stub stood in for.
RUN find src -type f -exec touch {} + && cargo build --release

# Runtime stage
FROM alpine:3.20

WORKDIR /app

# Install ca-certificates and other runtime deps
RUN apk add --no-cache ca-certificates libgcc

# Copy the binary from builder
COPY --from=builder /build/target/release/ollamaMQ /app/ollamaMQ

# Copy entrypoint script
COPY docker-entrypoint.sh /app/docker-entrypoint.sh
RUN chmod +x /app/docker-entrypoint.sh

# Expose the default port
EXPOSE 11435

# Set entrypoint
ENTRYPOINT ["/app/docker-entrypoint.sh"]
