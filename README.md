# Minijob (Rust)

Minijob is a job queue and task scheduling service with an HTTP API. It runs as a single
process, keeps its jobs in memory and mirrors them to a JSON file or to Redis.

This is the Rust implementation of Minijob, a drop-in replacement for the
[Go version](https://github.com/nbigot/minijob):

- same web API (routes, payloads, status codes, error codes and messages);
- same configuration files (the templates of the Go version work unchanged);
- same data (it reads the jobs file and the Redis keys written by the Go version);
- same command line (`-config`, `-version`) and the same Docker image layout.

Where the Go version had bugs, this version fixes them; [MIGRATION.md](MIGRATION.md) lists
every difference a Go user can notice.

## Build

Requirements: Rust 1.95 or later.

```sh
cargo build --release
./target/release/minijob -version
```

The version printed by `-version` is `v` + the crate version, or the value of the
`MINIJOB_VERSION` environment variable at build time:

```sh
MINIJOB_VERSION=v1.2.3 cargo build --release
```

Cargo features:

| Feature | Default | Effect |
|---|---|---|
| `redis` | yes | Redis backend (`backend.type: redis`) |
| `swagger-ui` | yes | Swagger UI under `/docs/` (without it, only `/docs/doc.json`) |
| `mimalloc` | no (yes in the Docker image) | mimalloc as the global allocator |

## Run

```sh
./target/release/minijob -config config-templates/standard/config/config.yaml
```

Templates in [config-templates](config-templates):

| Template | Backend | Notes |
|---|---|---|
| `standard/config/config.yaml` | in-memory, saved to `/data/minijob/jobs.json` | JSON-schema validation, retention policy |
| `standard/config/config.inmemory.yaml` | in-memory, not saved | quick tests |
| `standard/config/config.loadtest.yaml` | in-memory, not saved | load tests |
| `docker/config/config.yaml` | in-memory, saved to `/app/data/jobs.json` | used by the Docker image |

Relative paths in the configuration (JSON schema, storage directory, events log) are
relative to the working directory.

Quick check, with the server listening on port 8080:

```sh
$ curl http://localhost:8080
Welcome to MiniJob!
$ curl http://localhost:8080/livez
OK
$ curl -X POST http://localhost:8080/api/v1/job/ -H 'Content-Type: application/json' \
    -d '{"topic":"emails","properties":{"to":"someone@example.com"}}'
$ curl -X POST 'http://localhost:8080/api/v1/job/pull?topic=emails'
$ curl -X POST http://localhost:8080/api/v1/job/<id>/succeed
```

The API is described by the Swagger UI at `/docs/index.html` and by
[docs/swagger.yaml](docs/swagger.yaml).

Signals: SIGINT, SIGTERM and SIGQUIT stop the server gracefully (pending jobs are saved);
SIGHUP reloads the configuration and restarts the server in the same process.

The API has no authentication, including the admin routes (`/api/v1/admin/server/shutdown`,
`DELETE /api/v1/jobs/`): expose it only to trusted networks.

## Docker

```sh
docker build -t minijob --build-arg PROGRAM_VERSION=v1.0.0 .
docker run --rm -p 8080:8080 -v minijob-data:/app/data minijob
```

Same layout as the Go image: binary `/app/minijob`, configuration `/app/config/config.yaml`,
data `/app/data`, port 8080. The image is based on Alpine with a static (musl) binary built with
mimalloc (`--build-arg FEATURES=` uses musl's own allocator, slower under load, see
[bench/README.md](bench/README.md)); its health check calls `/livez`.

## Tests

```sh
cargo test
```

The unit tests are next to the code; [tests](tests) holds the integration tests of the web
API, one file per configuration template or storage backend, plus `security.rs`. They start
the server binary on a free port.

The Redis tests run only when `MINIJOB_TEST_REDIS_URL` points to a Redis server (they use and
empty database 9):

```sh
docker run -d --name redis -p 6379:6379 redis:7.4
MINIJOB_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test
```

Checks run by the CI:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
docker build .
```

## Documentation

- [MIGRATION.md](MIGRATION.md): differences with the Go version.
- [PORTING-NOTES.md](PORTING-NOTES.md): inventory of the Go code base (routes, configuration
  keys, data formats, concurrency) and the Go bugs found during the port.

## License

[MIT](LICENSE). The Swagger UI assets under [assets/swagger-ui](assets/swagger-ui) come from
[swaggo/files](https://github.com/swaggo/files) (MIT) and
[swagger-ui](https://github.com/swagger-api/swagger-ui) (Apache 2.0).
