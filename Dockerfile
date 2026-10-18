# Same image layout as the Go version: /app/minijob, /app/config/config.yaml, /app/data,
# port 8080, runs as root (volumes created by the Go image are owned by root).

FROM rust:1.97-alpine AS build
ARG PROGRAM_VERSION=v1.0.0
# Cargo features. The image uses mimalloc: musl's own allocator is up to twice as slow under
# load (bench/README.md). --build-arg FEATURES= builds with musl's allocator.
ARG FEATURES="mimalloc"
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
COPY docs ./docs
# Workspace member (the load generator); only its manifest is needed to build the server.
COPY bench ./bench
ENV MINIJOB_VERSION=${PROGRAM_VERSION}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --features "$FEATURES" && cp target/release/minijob /minijob

FROM alpine:3.22 AS runtime
RUN apk --no-cache add curl ca-certificates
WORKDIR /app
COPY --from=build /minijob /app/minijob
COPY config-templates/docker/config /app/config
EXPOSE 8080/tcp
# Go checked /api/v1/utils/ping, which does not exist (the container was never healthy).
HEALTHCHECK CMD curl -f http://localhost:8080/livez || exit 1
CMD ["/app/minijob", "-config", "/app/config/config.yaml"]
