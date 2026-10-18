#!/usr/bin/env bash
# Reproducible benchmark in Docker containers.
#
# Every server runs the same configuration (config-templates/standard/config/
# config.loadtest.yaml, plus the storage settings below), pinned to SERVER_CPUS; the load
# generator (and Redis) run on CLIENT_CPUS. Each run benchmarks every image on every
# storage, and the order of the images rotates from one run to the next.
#
#   bench/run.sh                                   # all variants, 3 runs, 10 s per scenario
#   RUNS=1 DURATION=5 VARIANTS=musl bench/run.sh   # quick check
#   IMAGES="a=img:tag b=other:tag" bench/run.sh    # benchmark existing images instead
#
# Settings (environment):
#   RUNS (3), DURATION (10, seconds per scenario), WARMUP (2), CONCURRENCY (32),
#   STORAGES ("inmemory file redis"), VARIANTS ("musl musl-system glibc"),
#   SERVER_CPUS (0,1), CLIENT_CPUS (2,3), OUT (bench/results/<date>),
#   IMAGES: space-separated label=image pairs to benchmark instead of building VARIANTS
#     (any image with the server at /app/minijob),
#   TOOL_IMAGE: load generator image (default: built from bench/Dockerfile).
# Results: one JSON file per run, storage and image, plus report.md and environment.txt.
set -euo pipefail
cd "$(dirname "$0")/.."
export MSYS_NO_PATHCONV=1

RUNS=${RUNS:-3}
DURATION=${DURATION:-10}
WARMUP=${WARMUP:-2}
CONCURRENCY=${CONCURRENCY:-32}
STORAGES=${STORAGES:-"inmemory file redis"}
VARIANTS=${VARIANTS:-"musl musl-system glibc"}
SERVER_CPUS=${SERVER_CPUS:-0,1}
CLIENT_CPUS=${CLIENT_CPUS:-2,3}
OUT=${OUT:-bench/results/$(date +%Y%m%d-%H%M%S)}
TOOL_IMAGE=${TOOL_IMAGE:-}
IMAGES=${IMAGES:-}
# Files go in and out of the containers with `docker cp` (no bind mount: Docker Desktop's
# file sharing is slow and fragile on Windows).
NET=minijob-bench
SERVER=minijob-bench-srv
CLIENT=minijob-bench-cli
REDIS=minijob-bench-redis

hostpath() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else echo "$1"; fi
}

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

if [ -z "$TOOL_IMAGE" ]; then
    TOOL_IMAGE=minijob-bench-tool
    docker build -q -t "$TOOL_IMAGE" -f bench/Dockerfile . >/dev/null
fi
if [ -z "$IMAGES" ]; then
    for v in $VARIANTS; do
        case $v in
            musl) docker build -q -t minijob-bench:musl . >/dev/null ;;
            musl-system) docker build -q --build-arg FEATURES= -t minijob-bench:musl-system . >/dev/null ;;
            glibc) docker build -q -t minijob-bench:glibc -f bench/Dockerfile.glibc . >/dev/null ;;
            *) echo "unknown variant $v" >&2; exit 2 ;;
        esac
        IMAGES="$IMAGES $v=minijob-bench:$v"
    done
fi
read -r -a PAIRS <<< "$IMAGES"

# One configuration per storage, from the load test template.
TPL=config-templates/standard/config/config.loadtest.yaml
for storage in $STORAGES; do
    cfg="$OUT/config-$storage.yaml"
    sed -e 's#address: "127.0.0.1:8080"#address: "0.0.0.0:8080"#' "$TPL" > "$cfg"
    case $storage in
        inmemory) ;;
        file)
            perl -0pi -e 's/        enablePersistentStorage: false\n/        enablePersistentStorage: true\n        writeFrequency: 30\n        directory: "\/app\/data"\n        filename: "jobs.json"\n/' "$cfg" ;;
        redis)
            perl -0pi -e "s/    type: \"inMemory\"\n/    type: \"redis\"\n    redis:\n        url: \"redis:\/\/$REDIS:6379\/0\"\n        maxJobs: 10000000\n        finishedJobTTL: 86400\n/" "$cfg" ;;
        *) echo "unknown storage $storage" >&2; exit 2 ;;
    esac
done

cleanup() {
    docker rm -f "$CLIENT" "$SERVER" "$REDIS" >/dev/null 2>&1 || true
}
trap cleanup EXIT
docker network create "$NET" >/dev/null 2>&1 || true

{
    echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "git: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "docker: $(docker version --format '{{.Server.Version}}') $(docker info --format '{{.OperatingSystem}}, kernel {{.KernelVersion}}, {{.NCPU}} CPUs, {{.MemTotal}} bytes')"
    echo "cpu: $(docker run --rm alpine:3.22 sh -c "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2" | sed 's/^ //')"
    echo "server cpus: $SERVER_CPUS, client cpus: $CLIENT_CPUS (Redis shares the client CPUs)"
    echo "runs: $RUNS, duration: $DURATION s, warm-up: $WARMUP s, concurrency: $CONCURRENCY"
    echo "load generator: $TOOL_IMAGE ($(docker image inspect -f '{{.Id}}' "$TOOL_IMAGE" | cut -c8-19))"
    for p in "${PAIRS[@]}"; do
        img=${p#*=}
        echo "image ${p%%=*}: $img ($(docker image inspect -f '{{.Id}}' "$img" | cut -c8-19), $(docker image inspect -f '{{.Size}}' "$img") bytes)"
    done
} > "$OUT/environment.txt"
cat "$OUT/environment.txt"

n=${#PAIRS[@]}
for run in $(seq 1 "$RUNS"); do
    for storage in $STORAGES; do
        for i in $(seq 0 $((n - 1))); do
            p=${PAIRS[$(( (i + run - 1) % n ))]}
            label=${p%%=*}
            image=${p#*=}
            cleanup
            if [ "$storage" = redis ]; then
                docker run -d --name "$REDIS" --network "$NET" --cpuset-cpus "$CLIENT_CPUS" \
                    redis:7.4 redis-server --save "" --appendonly no >/dev/null
            fi
            docker create --name "$SERVER" --network "$NET" --cpuset-cpus "$SERVER_CPUS" \
                "$image" /app/minijob -config /app/bench-config.yaml >/dev/null
            docker cp "$(hostpath "$OUT/config-$storage.yaml")" "$SERVER:/app/bench-config.yaml"
            docker start "$SERVER" >/dev/null
            echo "== run $run/$RUNS, $storage, $label ($image)"
            result="$storage-$label-run$run.json"
            docker create --name "$CLIENT" --network "container:$SERVER" --pid "container:$SERVER" \
                --cpuset-cpus "$CLIENT_CPUS" "$TOOL_IMAGE" \
                -url http://127.0.0.1:8080 -wait 60 -pid 1 -duration "$DURATION" -warmup "$WARMUP" \
                -concurrency "$CONCURRENCY" -label "$storage $label" -json /tmp/result.json >/dev/null
            if docker start -a "$CLIENT"; then
                docker cp "$CLIENT:/tmp/result.json" "$(hostpath "$OUT/$result")"
            else
                echo "!! benchmark failed: run $run, $storage, $label"
                # A failed consistency check still writes the result.
                docker cp "$CLIENT:/tmp/result.json" "$(hostpath "$OUT/$result")" 2>/dev/null || true
            fi
            docker rm "$CLIENT" >/dev/null
            docker logs "$SERVER" 2>&1 | grep -E '"level":"(error|fatal|panic)"|panic:' | head -5 || true
            docker stop -t 20 "$SERVER" >/dev/null
        done
    done
done

files=()
for f in "$OUT"/*.json; do files+=("/results/$(basename "$f")"); done
docker create --name "$CLIENT" "$TOOL_IMAGE" report "${files[@]}" >/dev/null
docker cp "$(hostpath "$OUT")/." "$CLIENT:/results"
docker start -a "$CLIENT" > "$OUT/report.md"
docker rm "$CLIENT" >/dev/null
echo "Report: $OUT/report.md"
