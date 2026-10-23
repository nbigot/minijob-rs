# Migrating from the Go version

The Rust version is a drop-in replacement: same routes, payloads, status codes, error codes
and messages, same configuration files, same storage formats, same command line and Docker
layout. You can stop the Go server and start this one on the same configuration and data.

This page lists what you can still notice, grouped by topic. Most items are bugs of the Go
version that are fixed here; the few deliberate behavior changes are marked **(change)**.
"Go" means Go minijob v1.0.0, the version this one was ported from.

## Data compatibility

- The jobs file of the in-memory backend and the Redis values have the same format and the
  same bytes. Files and keys written by Go are read as is, and Go can read what Rust writes.
- Integers in job `properties` keep their exact value. Go converted every number to a float,
  so integers above 2^53 were rounded (`12345678901234567890` became `12345678901234567000`).
  Floats are written like Go (`1.0` → `1`, `1e-7`).
- The jobs file is replaced atomically (temporary file, then rename). A crash during a save
  no longer leaves a truncated file that prevents the next start.
- A periodic save that fails (disk full, permissions) is logged (`Can't save json to file`)
  and tried again at the next one; Go logged it as fatal and exited, losing the jobs since
  the previous save.
- The final save always happens when the server stops. In Go, a change made during a
  periodic save could be skipped by the final save, and `docker stop` could kill the server
  before it (long polls delayed the shutdown).
- A job saved between its creation and its first scheduling (state `created`) is scheduled
  after a restart; Go left it there forever.

## Command line and process

- Exit status 1 on every startup or runtime error. Go exited with 0 when the backend, the
  schema, Consul or the listener (port already in use) failed.
- An invalid `logger` section or CORS origin is reported with a message and exit status 1;
  Go panicked (exit status 2 with a Go stack trace).
- **(change)** SIGHUP reloads the configuration and restarts the server, like the restart
  API (Go: the process was killed). SIGQUIT stops gracefully (Go: ignored). On Windows,
  Ctrl+C, Ctrl+Break and closing the console stop gracefully.
- `-h`, `-version`, flag errors and their exit statuses are unchanged.

## HTTP server

- Requests in progress finish when the server stops; long polls end at once with their
  usual "no job" answer, so `docker stop` completes quickly. In Go, requests arriving while
  the server was stopping through the admin API could crash it.
- A handler that panics answers 500; Go crashed the whole process.
- Request headers: up to 8 KiB are accepted; larger ones get `431` with an empty body. Go
  refused anything above 4 KiB with `431 Request Header Fields Too Large`, and the client
  often saw a connection reset instead of that answer.
- At most 100 header fields per request; more get `431` with an empty body (Go only limited
  the size).
- The request target must be a valid URI: `"`, `<` and `>` in the query, `<`, `>` and `` ` ``
  in the path, and bytes that are not UTF-8 must be percent-encoded. Such requests, and
  other malformed ones (invalid request line, header without colon, invalid
  `Content-Length`...), get `400` with an empty body; Go accepted these targets and
  described the other errors in a text body.
- An HTTP/1.0 request is answered with an HTTP/1.0 status line (Go: HTTP/1.1 and
  `Connection: close`).
- A client that does not send its headers within 10 s is disconnected (same timeout as Go).
- Bodies above 10 MiB get `413 Request Entity Too Large`, as in Go.
- Request bodies compressed with `gzip`, `deflate` or `br` (`Content-Encoding`) are decoded, as in Go.
  A body larger than 10 MiB once decoded gets `413 Request Entity Too Large`; Go decoded it
  without limit, so a few kilobytes of nested gzip could exhaust the server's memory.
- JSON nesting deeper than 128 levels is refused with the usual `400` (Go accepted up to
  10,000).
- `/debug/pprof/*` (Go profiles) is not available: 404. `/monitor` is available; its CPU and
  memory figures come from the operating system.
- `GET /api/v1/system/resources`: `memory` is the resident memory of the process (Go reported
  the size of its heap).
- Swagger UI: same page and `doc.json`; source maps and the ES-module bundles of swagger-ui
  are not embedded (404). The 404 message for an unknown file under `/docs/` shows the
  requested path (Go showed a garbled name), and so does the access log (Go logged what the
  Swagger handler left, such as `index.html` or an empty path). `X-Forwarded-Prefix` is
  honored on each request (Go used the value of the first request forever).
- HTTP header names are sent in title case; their order may differ.

## API behavior

- **(change)** `Content-Type: application/json; charset=utf-8` (or any parameter) is accepted
  when creating a job; Go required exactly `application/json`.
- **(change)** `POST /api/v1/resource/{name}/unlock` uses the name in the path when the
  `resourcename` query parameter is absent (Go ignored the path and answered HTTP 200 with
  `{"code":404,...}`). An unknown resource answers `404`
  `{"error":"resource not found: <name>","code":1030,...}`; Go answered `500 {}`.
- `search` in `GET /api/v1/jobs/` is case-insensitive in all cases (Go compared a full-length
  id case-sensitively, so an uppercase id found nothing).
- Sorting is stable: equal criteria are ordered by creation time, then by id. Go's order was
  random for equal values; `GET /api/v1/jobs/all` returns the jobs by id (Go: random order).
- The job returned when creating or cloning a job is shown as it was created (`pending` or
  `delayed`). Go built the answer while the scheduler could already be changing the job, so
  it sometimes showed `queued` (seen with the Redis backend).
- `GET /api/v1/topics/` lists topics in order of first use. After a restart, that is the
  creation order of the stored jobs (Go: random order after a restart).
- The topic of a pull that finds no job is recorded as sent. Go kept a reference to the
  request buffer, so later requests could overwrite it: `/api/v1/topics/`, the topic metrics
  and the `app_events` label showed garbage such as `01a1` instead of `none`.
- Concurrent requests on one job are consistent: a job can be completed once. Go had data
  races (two `succeed` or `fail` calls on the same job could both succeed).
- Pulls: a long poll (`waittimeseconds`) no longer blocks the other pulls, and it returns as
  soon as a job is queued (Go blocked every puller and checked once per second).
- **(change)** `backend.inMemory.maxJobs` and `backend.redis.maxJobs` are enforced: creating a
  job beyond the limit answers `503` `{"error":"cannot create job","code":1002}`, like
  `jobs.maxAllowedJobs`. Go stored the job in memory without saving it (in-memory) or
  ignored the limit (Redis).
- Changing the visibility timeout of a job is saved to Redis (Go only kept it in memory).
- Unchanged on purpose: an empty `topic` on pull only matches jobs without topic (`*` matches
  every topic); `visibilitytimeout` on an unknown job answers code 1007; deleting an unknown
  job answers 400; cloning does not check `jobs.maxAllowedJobs`.

## Scheduling and resource locks

- A lock is only released by the job that holds it. In Go, deleting a pending job, or the
  retention policy deleting a completed one, released the lock of another running job.
- A job retried after a delay locks its resources again before being queued; it waits while
  another job holds them. Go queued it without the lock.
- Duplicated names in `lockResources` are fine (Go left the job pending forever).
- Pending jobs are queued as soon as their resources are free, instead of at the next
  one-second tick.

## JSON schema validation

`validationErrors` (field, message, value) are the same as Go's. Differences:

- When several errors are found, their order is stable (sorted by property name). Go's order
  could change between requests.
- `$ref` is resolved inside the schema file, including draft-07 `#/definitions/...`, which
  never resolved in Go. Remote references are not downloaded (Go downloaded them while
  answering a request); they fail with the same message as an unreachable one in Go.
  `#name` references find a subschema with `"$anchor": "name"` or `"$id": "#name"` (Go
  ignored `$anchor`, and found such an `$id` only once validation had gone through it).
- `pattern`, `patternProperties` and `format: regex` use Go's regular expression syntax and
  matching (`\d`, `\w`, `\s`, `\b` are ASCII-only). Script names with an underscore
  (`\p{Old_Italic}`) are accepted; Go 1.25 refuses them.
- `format` checks accept the same strings as Go and give the same messages (they quote
  Go's `time.Parse`, `mail.ParseAddress` and `url.Parse`). The `contains` message shows the
  subschema as JSON; Go printed an internal structure with memory addresses.
- The schema is compiled at startup and shared safely between requests (Go modified it on
  first use from concurrent requests).
- A schema file Go refuses is refused with the same message, except a JSON syntax error,
  worded differently (`EOF while parsing an object at line 1 column 16`; Go: `unexpected
  end of JSON input`).

## Metrics

- `/api/v1/metrics/*` counters are corrected:
  - deleting a job decrements `jobsExisting`/`totalJobs` and increments
    `jobsCounterDeleted`/`deletedJobs` (Go never counted single deletes);
  - `resourcesLockedCount` decreases when locks are released (Go increased it);
  - jobs loaded at startup count in `totalJobs` and `activeJobs` (Go reported
    18446744073709551615 after a restart);
  - deleting all jobs resets the topic statistics;
  - a topic is listed once (Go could list it twice).
- The hidden event is named `Hidden` in the `job_events` metric, `jobActivity.hidden` and the
  event log (Go: `other`, so `hidden` stayed 0).
- `job_active_duration_seconds` is fed seconds (Go: milliseconds).
- Locked resources in `/api/v1/metrics/resources` are sorted by name; resources of deleted
  or hidden jobs are removed.
- With `webserver.metrics.enable: false` the server works; Go crashed about ten seconds after
  start.

## Storage backends

Redis:

- Loading uses `SCAN` and `MGET`; a key that expires during the load is skipped (Go failed to
  start).
- `CONFIG SET notify-keyspace-events` is attempted and only logged when refused (managed
  Redis); Go refused to start. The unused keyspace subscription and its stdout messages are
  gone.
- A failed write (Redis unreachable, or the write refused: out of memory, read-only replica)
  is logged and retried in the background until Redis accepts it; until
  then `/readyz`, `/healthcheck` and `/api/v1/system/health` answer 503. The API answers as
  Go did (the job exists in memory), after one Redis timeout (Go waited for every go-redis
  retry: 18 s with the template URL instead of 6 s). In Go a failed write was only logged:
  the job reached Redis only if a later change to the same job was saved.
- All go-redis URL options are accepted; `client_name` is set on the first connection.
- No `Error closing Redis client` line at shutdown (Go stopped the provider twice and logged
  `redis: client is closed`).

## Callbacks

- A callback is sent when a job becomes succeeded, failed or canceled, with the job as it was
  at that moment. Go built the payload later (a canceled job was usually reported `queued`)
  and resent the callbacks of every completed job after each restart.
- At most 256 callbacks are sent at the same time.
- `Callback failed` errors are Go's for a URL Go refuses (`parse ...`, `unsupported protocol
  scheme`, `no Host in request URL`); a network error gives its cause in other words
  (`tcp connect error: ...`, `dns error: ...`; Go: `dial tcp ...`).
- The JSON schema of the `standard` and `docker` templates (`schema.json`) lists
  `callbackUrl`. The Go schema did not and refuses additional properties, so with schema
  validation enabled every job with a callback was rejected; add `callbackUrl` to a schema
  copied from the Go templates.

## Event log

- Same lines. A quote, backslash or control character in a topic is escaped, so each line
  stays valid JSON; Go wrote the topic as is. Other characters are written as is, like Go.
- After a restart, the `Loaded` lines follow the order of the stored jobs (Go: random).
- The hidden event is written `Hidden` (Go: `other`, see Metrics).
- A write error is logged instead of crashing the server.

## Logs

- Same zap configuration and JSON line layout. When `disableCaller: false`, the caller is a
  Rust source file and the function name is empty (an empty column with the console
  encoding); with `disableStacktrace: false` the stack trace is the Rust one. YAML syntax
  error messages differ.
- With the JSON encoding, a `callerKey` without `callerEncoder` and `disableCaller: false`
  crashed the Go server at its first log line (nil pointer); the port writes the short
  caller.
- The time zone abbreviation (`MST` in a `timeEncoder` layout, and in the `NextRetryTime`
  field of verbose logs) is written as the offset, `+0200` (`UTC` when the offset is zero);
  Go writes the abbreviation of the local zone when it knows it (`CEST`).
- The access log (`webserver.logs.enable`) has the same format.
- Same messages and fields. When the admin API stops the server, `Service stopped` comes
  after `Server stopped`: the job service stops once the HTTP server has finished (Go stopped
  it first, while requests were still accepted).
- When the server cannot start (`Cannot initialize server`: schema file, CORS origins,
  Consul registration), the job service has not started, so there are no `Service started`
  and `Service stopped` lines (Go started it first).
- A failed request to the Consul agent gives its cause in other words
  (`Put "<url>": tcp connect error: ...`; Go: `Put "<url>": dial tcp ...`).

## Docker image

- Health check on `/livez`. The Go image checked `/api/v1/utils/ping`, which does not exist,
  so the container was never healthy.
- Static musl binary on Alpine 3.22; same paths, port and command. The image is 23.9 MB
  (Go: 58.2 MB).
- `docker stop` takes well under a second, also while a long poll is waiting; the poll gets
  its usual "no job" answer and the jobs are saved. The Go image was killed by Docker
  (exit code 137) in that case, without its final save.
- A data volume written by the Go image can be used as is.

## Configuration notes

- Retry delays (`jobs.retryPolicy.backoffStrategy.base` and `maxDuration`) are milliseconds,
  as in the Go code. The shipped templates set `base: 2` and `maxDuration: 60` without `type`
  (constant), so the delay between retries is 2 ms. For 2 s, then 4 s, 8 s... up to one
  minute, use:

  ```yaml
  retryPolicy:
      maxRetry: 3
      backoffStrategy:
          type: 2          # 0 constant, 1 linear, 2 exponential
          base: 2000       # milliseconds
          factor: 2
          maxDuration: 60000
  ```

- A negative `backend.inMemory.writeFrequency` is a configuration error (Go panicked).
- CORS origins: entries of `allowOrigins` may have spaces after the commas, wildcard
  subdomains included (`"https://a.com, https://*.b.com"`); Go never matched the wildcard in
  that case.
- Consul: same registration, agent configured by the `CONSUL_HTTP_*` environment variables;
  `unix://` agent addresses are not supported. The service is deregistered with
  `consul.server.token`, like the registration (Go used only `CONSUL_HTTP_TOKEN`, so with
  ACLs and the token in the configuration the stopped service stayed registered).

## How compatibility was checked

- The same scripted session (about 240 requests: every route, error cases, job lifecycle,
  retries, locks, compressed bodies, restart) was sent to Go v1.0.0 and to this version with
  each shipped configuration template and with each backend (in-memory with and without
  file, Redis 7.4, mock), once with the event log and the access log enabled, and once with
  metrics disabled. Answers were compared after masking job ids, timestamps and durations;
  every remaining difference is described on this page.
- A Redis outage was simulated (Redis stopped, a job created, Redis started again): the job
  reaches Redis once it is back, and `/readyz` answers 503 in between.
- Data written by each version (jobs file, Redis keys) was loaded by the other one: same
  answers, and the data was left byte for byte as written.
- Both Docker images ran the same session in UTC containers, and a volume written by the Go
  image was used by the Rust image.
- The Go repository's load test client (`cmd/loadtest`) runs unchanged against this version
  (3,000 jobs with the `monkey` scenarios, no error).
