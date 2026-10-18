# minijob-bench

Load generator for the minijob HTTP API, and a script that benchmarks server builds in
Docker containers. The [Go repository](https://github.com/nbigot/minijob) has the same tool
(`cmd/benchmark`) with the same options, scenarios and result files, so either tool can
measure either server and `report` accepts files written by both.

## Scenarios

Each scenario starts from an empty server (`DELETE /api/v1/jobs/`).

| Scenario | What one item is | Notes |
|---|---|---|
| `ping` | `GET /ping` | trivial request |
| `create` | `POST /api/v1/job/` (about 130 bytes) | small writes; checks that every created job is stored |
| `pull-batch` | `POST /api/v1/job/pull?numjobs=100` | batch writes: `-batch-jobs` jobs are created first (not measured), then pulled until the queue is empty; checks that every job is pulled exactly once |
| `get` | `GET /api/v1/job/{id}` | reads, on `-preload` jobs |
| `list` | `GET /api/v1/jobs/?page=N&limit=100` | reads (random page) |
| `list-filter` | `GET /api/v1/jobs/?status=queued&topic=tN&sort=priority_desc&limit=20` | reads with filters |
| `cycle` | create, pull (until the job is queued), succeed | the life of a job; latency is per cycle; checks that every cycle's job ends succeeded |

Timed scenarios run `-concurrency` connections in a closed loop (one request at a time per
connection) for `-warmup` seconds, then are measured for `-duration` seconds.

## Running the tool

```sh
cargo run --release -p minijob-bench -- -url http://127.0.0.1:8080 -json result.json
cargo run --release -p minijob-bench -- report run1.json run2.json run3.json
```

Options: `-url`, `-scenarios` (comma-separated or `all`), `-concurrency` (32), `-duration`
(10), `-warmup` (2), `-preload` (10000), `-batch-jobs` (100000), `-batch-size` (100),
`-pid` (server process id, Linux: CPU time per request and memory from `/proc`), `-label`,
`-wait` (seconds to wait for `/ping`), `-json`. The exit status is 1 when a consistency
check fails.

Result file: one object per run with, for each scenario, requests, errors, requests/s,
items/s, latency (mean, p50, p90, p99, max in ms, nearest-rank percentiles over every
sample), HTTP statuses, server CPU µs per request, server RSS, load generator CPU and the
consistency check.

## Benchmark in Docker

```sh
bench/run.sh                                     # 3 runs, every storage, every build
RUNS=1 DURATION=5 VARIANTS=musl bench/run.sh     # quick check
IMAGES="a=image:tag b=other:tag" bench/run.sh    # existing images instead of builds
```

- Builds: `musl` (the release image: musl with mimalloc), `musl-system` (musl with its own
  allocator, `--build-arg FEATURES=`), `glibc` (`bench/Dockerfile.glibc`, glibc allocator).
- Storages: `inmemory` (load test template), `file` (same, with the jobs file in the
  container, written every 30 s), `redis` (Redis 7.4 without persistence).
- The server is pinned to CPUs 0–1 and the load generator to CPUs 2–3 (`SERVER_CPUS`,
  `CLIENT_CPUS`); every server uses `config-templates/standard/config/config.loadtest.yaml`
  with only the storage settings changed. The order of the images rotates between runs.
- Results go to `bench/results/<date>/`: one JSON file per run, `report.md` (median and
  min–max over runs) and `environment.txt` (machine, Docker, image ids).

## Measurement limits

- One machine: the load generator, Redis and the server share the same memory bus and
  caches, and under Docker Desktop they run in a virtual machine.
- Redis shares the load generator's CPUs.
- When the load generator's CPU is close to 200% (2 CPUs), it is the limit, not the server.
- CPU time comes from `/proc/<pid>/stat` with a 10 ms resolution, averaged over the window.
- `pull-batch` is not timed: it ends when the queue is empty, so a fast server is measured
  over a shorter time (less than a second for 100,000 jobs) and its figures vary more.
- Closed loop: the latencies are those of a server under full load (no coordinated
  omission correction); they are not the latencies of a lightly loaded server.

## Results

Measured on 2026-10-18: Intel Core i5-6500 (4 cores), Docker Desktop 29.8 (Linux VM with 4
CPUs and 3.8 GiB), server on 2 CPUs, load generator on the 2 others, 32 connections, 3 runs
of 10 s per scenario. The values are medians. During the runs the host stayed idle (5% CPU
on average, sampled every 15 s).

Throughput (items per second: requests; jobs for `pull-batch`; cycles for `cycle`):

| Scenario | musl + mimalloc (release image) | musl, its own allocator | glibc |
|---|---:|---:|---:|
| **in-memory** | | | |
| ping | 70,530 | 69,936 | 73,610 |
| create | 55,278 | 30,055 | 52,727 |
| pull-batch | 98,129 | 90,606 | 93,451 |
| get | 62,388 | 57,023 | 63,252 |
| list | 1,595 | 909 | 1,530 |
| list-filter | 4,772 | 2,313 | 3,495 |
| cycle | 15,574 | 15,225 | 16,258 |
| **Redis** | | | |
| ping | 71,236 | 65,268 | 67,418 |
| create | 25,810 | 15,606 | 25,222 |
| pull-batch | 56,301 | 59,867 | 56,005 |
| get | 66,632 | 35,078 | 64,375 |
| list | 1,542 | 900 | 1,336 |
| list-filter | 4,239 | 2,278 | 3,379 |
| cycle | 7,532 | 6,924 | 7,459 |

With the jobs file, the mimalloc image gives the same figures as in-memory (within 5%). The
other two vary more (up to 12% lower for musl's allocator, 25% for glibc on filtered
lists), probably because the file is written every 30 s: a save that falls in a measured
window takes CPU from it.

| | musl + mimalloc | musl, its own allocator | glibc |
|---|---:|---:|---:|
| Memory at startup (RSS) | 13.3 MiB | 5.1 MiB | 7.7 MiB |
| Memory per stored job (peak RSS above startup / jobs created) | 1,500 B | 1,600 B | 1,630 B |
| Image size | 23.9 MB | 23.8 MB | 104.7 MB |
| Requests in error, failed consistency checks | 0 | 0 | 0 |

Conclusions:

- musl's own allocator is the limit as soon as requests allocate a lot: mimalloc creates jobs
  1.8 times faster, lists them 1.75 times faster (filtered lists: 2 times), and reads them 1.9
  times faster on Redis. The release image is therefore built with mimalloc.
- glibc's allocator is about as fast as mimalloc, except for filtered lists (20–27% slower),
  but the image is 4.4 times larger (Debian, dynamic binary).
- mimalloc keeps 8 MiB more at startup, and no more per job.
- `ping`, `cycle` and `pull-batch` are the same with every allocator: they are limited by the
  round trip of the requests (closed loop) or by the load generator (about 170% of its 2 CPUs
  during `pull-batch`), not by the server.
- The same mimalloc image measured in a second series gave figures within 5% of these.
