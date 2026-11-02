FROM node:22-bookworm-slim AS web
WORKDIR /src/apps/controller
RUN corepack enable && corepack prepare pnpm@10.26.0 --activate
COPY apps/controller/package.json apps/controller/pnpm-lock.yaml apps/controller/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY apps/controller/ ./
# Keep the repository layout: the controller imports generated Rust wire types
# and its typechecked tests import the shared WebCrypto fixture.
COPY crates/protocol/bindings/ /src/crates/protocol/bindings/
COPY crates/host/tests/fixtures/webcrypto-vector.json /src/crates/host/tests/fixtures/webcrypto-vector.json
RUN pnpm build

FROM rust:1.96-bookworm AS rust
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo build --locked --release -p lantern-signaling

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 lantern
WORKDIR /app
COPY --from=rust /src/target/release/lantern-signaling /usr/local/bin/
COPY --from=web /src/apps/controller/dist /app/web
USER lantern
ENV LANTERN_BIND=0.0.0.0:8787 LANTERN_WEB_ROOT=/app/web
EXPOSE 8787
CMD ["lantern-signaling"]
