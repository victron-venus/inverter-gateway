# Multi-stage Dockerfile for inverter-gateway
# Stage 1: build
FROM rust:1.88-alpine@sha256:9dfaae478ecd298b6b5a039e1f2cc4fc040fc818a2de9aa78fa714dea036574d AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app

# Cache deps
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --locked --release && \
    rm -rf src

COPY . .
RUN touch src/main.rs && cargo build --locked --release

# Stage 2: minimal runtime
FROM alpine:3.24@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
RUN apk add --no-cache ca-certificates tini
RUN adduser -D -g '' appuser
COPY --from=builder /app/target/release/inverter-gateway /usr/local/bin/inverter-gateway
USER appuser
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/inverter-gateway"]
EXPOSE 8080 8443
