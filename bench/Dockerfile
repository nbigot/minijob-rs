# Load generator image: docker build -t minijob-bench-tool -f bench/Dockerfile .
FROM rust:1.97-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
COPY docs ./docs
COPY bench ./bench
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p minijob-bench && cp target/release/minijob-bench /minijob-bench

FROM alpine:3.22
COPY --from=build /minijob-bench /usr/local/bin/minijob-bench
ENTRYPOINT ["minijob-bench"]
