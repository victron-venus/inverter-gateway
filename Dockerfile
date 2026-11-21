# Multi-stage Dockerfile for inverter-gateway
# Stage 1: build
FROM rust:1.100-alpine@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 AS builder
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
FROM alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
RUN apk add --no-cache ca-certificates tini && \
    adduser -D -g '' appuser
COPY --from=builder /app/target/release/inverter-gateway /usr/local/bin/inverter-gateway
USER appuser
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/inverter-gateway"]
EXPOSE 8080 8443
