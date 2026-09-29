# syntax=docker/dockerfile:1

# The dashboard: a static SPA that the server embeds.
FROM node:24-slim AS web
WORKDIR /app/web
RUN npm install --global pnpm@10
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

# The server. cargo-chef builds the dependencies in their own layer, so a code change does not
# build them again. OpenSSL (for passkeys) is built from source: the image has perl and make.
FROM rust:1.98-trixie AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app/server

FROM chef AS planner
COPY server/ ./
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/server/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY server/ ./
COPY --from=web /app/web/dist/client /app/web/dist/client
RUN cargo build --release --locked && mkdir -p /out/data

# Only the binary, glibc and CA certificates. The user is nonroot (65532).
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /app/server/target/release/aiproxy /usr/local/bin/aiproxy
COPY --from=builder --chown=65532:65532 /out/data /data
ENV AIPROXY_DATA_DIR=/data \
    AIPROXY_BIND=0.0.0.0:8080
EXPOSE 8080
VOLUME /data
ENTRYPOINT ["/usr/local/bin/aiproxy"]
CMD ["serve"]
