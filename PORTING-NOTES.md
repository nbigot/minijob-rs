# Porting notes: minijob (Go) → Rust

Working notes for the Rust port. Phase 1 (inventory) — 2026-10-16.

- Source: Go repository at commit `11f79ef` (branch `main`).
- Toolchains on the dev machine: go1.27.2 (go.mod says `go 1.25.3`), rustc/cargo 1.97.0, Docker 29.8.
- Legend: **[verify]** = behavior read from library source, to be confirmed against the
  running Go server in phase 3. `file:line` references are to the Go repository.

---

## 0. Scope findings

What the program is: a single-process job queue with an HTTP API (Fiber v2 / fasthttp). Jobs
live in memory in the service; a *backend provider* mirrors them to storage. Three providers:

| `backend.type` | Storage | Notes |
|---|---|---|
| `inMemory` | none, or one JSON file rewritten every `writeFrequency` s | `enablePersistentStorage` |
| `redis` | one Redis string per job, `job:<uuid>` | write-through on every event |
| `mock` | none | hard-coded limits, logs every event at debug level |

Items of the porting checklist that **do not exist** in this code base:

- no authentication, JWT, RBAC/ABAC, API keys or rate limiter. **Every route, including
  `POST /api/v1/admin/server/shutdown` and `DELETE /api/v1/jobs/`, is unauthenticated.**
- no SQL database (no MySQL, no tables, no transactions); the only external store is Redis.
- no jq filters.

Consequences for the default stack: `sqlx` and `jaq` are not needed; `redis` (redis-rs) and
`reqwest` (callbacks, Consul) are.

Size: 13,986 Go lines in total = 8,416 lines of program code + 2,738 lines of tests +
1,824 lines of generated `docs/docs.go` + 1,008 lines of client tools (`cmd/loadtest`,
`cmd/benchmark`).

---

## 1. Command line and process

`cmd/minijob/minijob.go`, Go `flag` package (single-dash and double-dash both accepted,
`-flag value` and `-flag=value`):

| Flag | Default | Effect |
|---|---|---|
| `-config string` | `config.yaml` | path to the YAML configuration |
| `-version` | false | prints `Version: <v>` to stdout, exit 0 |
| `-h`, `-help` | | usage on stderr, exit 0 |
| unknown flag | | error + usage on stderr, exit 2 |

Usage text produced by Go (program name = `os.Args[0]`):

```
Usage of <argv0>:
  -config string
    	Filepath to config.yaml (default "config.yaml")
  -version
    	Show version
```

Version: `main.Version`, default `v0.0.0`, set with `-ldflags "-X main.Version=v1.2.3"`
(Dockerfile build-arg `PROGRAM_VERSION`, goreleaser `v{{Major}}.{{Minor}}.{{Patch}}`).

Main loop (`main()`): load config → `RunServer` → if a restart was requested (API
`/api/v1/admin/server/restart`) reload the config file and start again **in the same
process**; otherwise exit.

Exit status as implemented:

| Situation | Go exit status | Output |
|---|---|---|
| `-version`, `-h` | 0 | |
| config file missing / YAML error | 1 | `error while opening configuration file <f> : <err>` or `error while parsing configuration file <f> : <err>` on **stdout** |
| invalid/missing `logger` section | 2 (panic) | `Error while initializing logger` + Go panic trace |
| invalid CORS origin | 2 (panic) | |
| unknown `backend.type`, `maxJobs: 0`, bad Redis URL, Redis unreachable, schema file unreadable, Consul registration error, **listen error (port in use)** | **0** (bug B01) | zap error log |
| SIGINT / SIGTERM / API shutdown | 0 | |
| runtime panic (see bugs B02, B03, B07) | 2 | |

Signals (`web/webserver/webserver.go:67`): `signal.Notify(SIGINT, SIGTERM, SIGQUIT)`.
SIGINT/SIGTERM → graceful stop. SIGQUIT → caught and ignored. SIGHUP → handled in the
`switch` as "reload and restart" but **not registered**, so a real SIGHUP kills the process
(bug B17). The restart API injects a fake SIGHUP into the channel.

---

## 2. HTTP server

`web/config.go` — Fiber config:

| Setting | Value | Port note |
|---|---|---|
| StrictRouting | true | `/api/v1/job` ≠ `/api/v1/job/` |
| CaseSensitive | true | |
| UnescapePath | false | routing and `Params()` see the raw, percent-encoded path |
| BodyLimit | 10,485,760 B | over the limit → 413 [verify body] |
| Concurrency | 262,144 connections | beyond → 503 [verify] |
| ReadTimeout / WriteTimeout / IdleTimeout | 10 s each | [verify] that a 60 s long-poll is not cut by WriteTimeout |
| Read/WriteBufferSize | 4096 | header size limit 4 KiB → `431 Request Header Fields Too Large` (verified; the client often sees a reset instead). Rust: hyper minimum is 8 KiB, beyond → 431 with empty body |
| ETag, DisableKeepalive | false | |
| Server header | none (Fiber sets `NoDefaultServerHeader`) | `Date` header present |
| HTTP versions | HTTP/1.0 and 1.1 only (no h2/h2c) | |

fasthttp normalizes request paths (`//`, `/./`, `/../`) before routing [verify].

Default responses produced by Fiber (must be reproduced):

| Case | Status | Content-Type | Body |
|---|---|---|---|
| no route matches | 404 | `text/plain; charset=utf-8` | `Cannot <METHOD> <html-escaped original path>` |
| path exists for another method | 405 | `text/plain; charset=utf-8` | `Method Not Allowed` [verify Allow header] |
| `c.SendStatus(200)` with empty body | 200 | `text/plain; charset=utf-8` | `OK` |
| `c.SendStatus(503)` | 503 | idem | `Service Unavailable` |
| `c.JSON(v)` | | `application/json` (no charset) | `encoding/json` output, no trailing newline |
| `c.SendString(s)` | | `text/plain; charset=utf-8` | |

GET routes also answer HEAD (Fiber registers both).

---

## 3. Middleware chain (registration order = execution order)

`cmd/minijob/minijob.go:118`, then `web/webapiserver.go:AddRoutes`:

1. **Access log** (`webserver.logs.enable`): Fiber logger to stdout, format
   `[${time}] ${status} - ${ip}:${port} - ${latency} ${method} ${path}\n`, time format
   `2006-01-02T15:04:05-0700` local time, refreshed every 500 ms; `${port}` is the
   *client* port; `${latency}` is a Go duration padded with `%13v` (e.g. `"     12.345µs"`);
   `${status}` is colored only when stdout is a terminal.
2. **CORS** (`webserver.cors.enable`), Fiber v2.52 `cors` (`middleware/cors/cors.go`):
   - `allowOrigins` = `*` or comma-separated origins (each normalized `scheme://host[:port]`,
     lowercase; `https://*.example.com` subdomain wildcards); an invalid origin panics at start.
   - default AllowMethods `GET,POST,HEAD,PUT,DELETE,PATCH`; `allowHeaders` empty → reflect
     `Access-Control-Request-Headers`; no credentials, no expose headers, no max-age.
   - no `Origin` header → pass (adds `Vary: Origin` unless `*`).
   - `OPTIONS` without `Access-Control-Request-Method` → `Vary: Origin`, pass to router.
   - simple request → `Access-Control-Allow-Origin` (`*` or the origin), pass.
   - preflight → `Vary: Access-Control-Request-Method, Access-Control-Request-Headers, Origin`,
     allow-origin/methods/headers, **204** with empty body (never reaches the router).
   - unknown origin → no allow-origin header, request still served.
3. **Prometheus** (`webserver.metrics.enable`): registers `GET /metrics` *before* the API
   routes, then a middleware that times every request and records it **only if the handler
   set `c.Locals("metricName")`**. The `method` label is read before `c.Next()`, i.e. the
   middleware's own route method (`USE`) [verify]. `SetSkipPaths` is set but never used.
4. **API routes** (section 4).
5. `GET /ping`, `GET /healthcheck`.
6. **Healthcheck** (`app.Use`, so after all routes): `GET /livez` → 200 `OK` always;
   `GET /readyz` → backend `Healthcheck()` ? 200 `OK` : 503 `Service Unavailable`.
   Non-GET → passes through → 404/405.
7. **Monitor** (`webserver.monitor.enable`): `GET /monitor` (Fiber monitor: HTML page, JSON
   when `Accept: application/json`: `{"pid":{"cpu","ram","conns"},"os":{"cpu","ram",
   "total_ram","load_avg","conns"}}`) and **pprof** under `/debug/pprof/*` (Go runtime
   profiles).
8. **Swagger** (`webserver.swagger.enable`): `GET /docs/*` (gofiber/swagger v1.1.1):
   `/docs/index.html` (template, doc URL `/docs/doc.json`), `/docs/doc.json` (the swag doc,
   `Content-Type: application/json`), `/docs/` → 301 to `/docs/index.html`, other names →
   swagger-ui static files from `swaggo/files/v2 v2.0.2`, unknown file → 404. A second
   swagger handler with `URL: "swagger.json"` is registered on the same path and only runs
   when a file is not found.

No panic recovery middleware: a panic in a handler kills the process.

---

## 4. Routes

Path parameters: `jobuuid`, `resourcename`. All query parameters are read with `c.Query`
(first value, URL-decoded). Integer query parameters use `GetUintParameterFromQuery`
(`web/apihelpers.go:54`): empty or missing → default; otherwise `strconv.ParseUint(s, 10, 32)`
(no sign, no spaces, max 4294967295), then min/max checks. Errors (400, code 1021):

- parse error: `invalid value for query parameter: <name>`
- below min: `invalid value for query parameter: <name>, found <raw>, minimum allowed value is <min>`
- above max: `invalid value for query parameter: <name>, found <raw>, maximum allowed value is <max>`

Job UUID parsing (`uuid.Parse`): accepts `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`,
`{…}`, `urn:uuid:…` (prefix case-insensitive), 32 hex digits; any case. Invalid → 400:
`{"error":"invalid job uuid","code":1000,"jobUUID":"00000000-…","validationErrors":[{"failedField":"jobuuid","tag":"parameter","value":"<raw param>"}]}`
(the value is the raw, still percent-encoded path segment).

Success envelopes: `{"code":200,"message":"success", ...}` (`web/apiresult.go`).

### 4.1 Jobs

| Method & path | Handler | Input | 200 body | Errors (HTTP, code, message) |
|---|---|---|---|---|
| `GET /api/v1/job/:jobuuid` | GetJob | | `{code,message,job:JobResponse}` | 400/1000 invalid uuid; **404**/1001 `job not found` |
| `POST /api/v1/job/` | CreateJob | body JSON `JobRequest`, `Content-Type` must be exactly `application/json` (lowercased compare) | `{code,message,job}` | 400/1024 `invalid content type (must be application/json)`; schema on: 400/1022 `invalid json body format` (not JSON), 400/1023 `invalid json content` + `validationErrors`; 400/1002 `cannot create job from request` (decode error); 503/1002 `cannot create job` (`jobs.maxAllowedJobs` reached); 500/1002 `cannot create job` (uuid generation) |
| `POST /api/v1/job/pull` | PullJob | query `numjobs` (1, 1..100), `visibilitytimeout` (default `jobs.defaultVisibilityTimeout`, 1..`jobs.maxVisibilityTimeout`), `waittimeseconds` (0, 0..60), `topic`, `jobuuid` | `{code,message,jobs:[JobResponse]}` | 400/1021 params; 400/1000 bad `jobuuid`; 404/1004 `no jobs found in the queue` (no job at all) or `no job found that is ready or matches the criteria`; specific job: 400/1005 `job not found in the queue`, 400/1006 `job not found` / `job already started` |
| `POST /api/v1/job/:jobuuid/succeed` | SetJobAsSuccessful | | `{code:200,message:"success"}` | 400/1009 `job not found`, `job is not running` |
| `POST /api/v1/job/:jobuuid/cancel` | CancelJob | | success | 400/1008 `job not found`, `job is not running` |
| `POST /api/v1/job/:jobuuid/fail` | FailJob | | `message`: `success`, or `job reached max retry` when terminated | 400/1010 `job not found`, `job is not running` |
| `POST /api/v1/job/:jobuuid/visibilitytimeout` | ChangeVisibilityTimeoutJob | query `visibilitytimeout` (same rule as pull) | success | 400/1021; 400/**1007** `job not found` (bug B16c); no state check |
| `POST /api/v1/job/:jobuuid/clone` | CloneJob | | `{code,message,job}` (new job) | 400/1007 `job not found`; maxAllowedJobs is **not** checked |
| `DELETE /api/v1/job/:jobuuid` | DeleteJob | | success | **400**/1003 `job not found` |
| `GET /api/v1/jobs/` | GetJobs | `page` (1, 1..10000), `limit` (10, 1..1000), `status`, `topic`, `sort`, `search` | `{code,message,jobs,total,page,limit,totalpages}` | 400/1021 |
| `GET /api/v1/jobs/all` | GetAllJobs | | `{code,message,jobs}` (map order: random) | |
| `GET /api/v1/jobs/oldest` | GetOldestJobs | `limit` (10, 1..100), `duration` s (0, 0..31536000) | `{code,message,topics:{topic:[{jobuuid,duration}]}}` | 400/1021 |
| `GET /api/v1/jobs/stalled` | GetStalledJobs | `duration` min (60, 1..10080) | `{code,message,jobs:[{id,topic,state,created,duration}]}` longest first | 400/1021 |
| `GET /api/v1/jobs/recent` | GetRecentJobs | `duration` min (10, 1..10080) | idem, shortest first | 400/1021 |
| `DELETE /api/v1/jobs/queued` | DeleteQueuedJobs | | success | first DeleteJob error |
| `DELETE /api/v1/jobs/` | DeleteAllJobs | | success | |

Details:

- `GetJobs`: `status` is one of `created, delayed, pending, queued, running, succeeded,
  failed, canceled, deleted, hidden` (case-insensitive); an unknown status returns no job
  (not an error). `topic` exact match (`*` is not special here). `search` = substring of the
  job id (see bug B16d). `sort` = comma-separated list of `created_asc, created_desc
  (default), priority_asc, priority_desc, updated_asc, updated_desc, topic_asc, topic_desc`,
  unknown criteria ignored, final tie-break creation ms ascending; `sort.Slice` is unstable.
  `totalpages = ceil(total/limit)`; a page past the end returns `jobs: []`.
- `oldest`: non-completed jobs, age = now − creation; `stalled`/`recent`: queued or running
  jobs, age compared in minutes; `duration` field is milliseconds.
- `/api/v1/jobs/stalled` and `/api/v1/metrics/jobs/stalled` (and `recent`) are the same
  handlers.

### 4.2 Resources, topics, admin, system

| Method & path | Response |
|---|---|
| `GET /api/v1/topics/` | `{code,message,topics:[string]}` — topics known to the metrics module (insertion order) |
| `GET /api/v1/resources/` | `{code,message,resources:{name:jobUUID}}` |
| `POST /api/v1/resources/unlock` | success (error → 500 `{}`) |
| `POST /api/v1/resource/:resourcename/unlock` | reads **`?resourcename=`**, not the path (bug B16a). Missing → **HTTP 200** `{"code":404,"message":"missing or empty resource name in query","data":null}`; unknown resource → 500 `{}`; else success |
| `POST /api/v1/admin/server/shutdown` | stops the service synchronously, requests shutdown, success |
| `POST /api/v1/admin/server/restart` | requests restart (config reload), success |
| `GET /api/v1/system/info` | `{code,message,systemInfo:{version,uptime(s),hostname(= webserver.http.address),environment}}` |
| `GET /api/v1/system/health` | `OK` / `Service Unavailable` (text) |
| `GET /api/v1/system/resources` | `{code,message,resourceUsage:{memory:{label:"Memory Usage",value:"<n>MB"},disk:{label:"Disk Usage",value:"<n>MB"},backend:{label:"Backend Provider",value:"InMemory"\|"Redis"\|"Mock"}}}` — memory is Go heap `Alloc`; disk is the size of the in-memory file (0 otherwise) |
| `GET /` | `Welcome to MiniJob!` (README says `Welcome to minijob!`) |
| `GET /ping`, `GET /healthcheck` | `OK`; `/healthcheck` → 503 when the backend is unhealthy |
| `GET /livez`, `GET /readyz` | see section 3 |

### 4.3 Metrics routes (only when `webserver.metrics.enable`)

| Route | Body |
|---|---|
| `GET /metrics` | Prometheus text exposition (section 12) |
| `GET /api/v1/metrics/jobs/processing` | `{code,message,jobStats:{existingJobs,createdJobs,delayedJobs,pendingJobs,queuedJobs,runningJobs,succeededJobs,failedJobs,hiddenJobs,canceledJobs,deletedJobs}}` |
| `GET /api/v1/metrics/jobs/activity` | `{code,message,jobActivity:{created,delayed,pending,enqueued,started,succeeded,failed,canceled,hidden,deleted,timeout,terminated}}` (from the `job_events` counter) |
| `GET /api/v1/metrics/jobs/topics` | `{code,message,metrics:{topic:[JobMetrics]}}` — one-element array per topic; JobMetrics fields: `resourcesLockedCount, jobsCounterDeleted, jobsExisting, jobsStatusCreated, jobsStatusDelayed, jobsStatusPending, jobsStatusQueued, jobsStatusRunning, jobsStatusSucceeded, jobsStatusFailed, jobsStatusHidden, jobsStatusCanceled` |
| `GET /api/v1/metrics/resources` | `{code,message,resources:[{resourceName,jobUuid,topic,startTime(s),lockDuration(s)}]}` (random order) |
| `GET /api/v1/metrics/topics` | `{code,message,topics:[{topicName,totalJobs,percentJobs,activeJobs,successRate,averageDuration(s)}]}` |
| `GET /api/v1/metrics/jobs/recent`, `/jobs/stalled` | same as `/api/v1/jobs/recent`, `/stalled` |

---

## 5. Error model

`web/apierror/apierror.go`:

```go
type APIError struct {
    HttpCode int                `json:"-"`
    Err      error              `json:"-"`
    Message  string             `json:"error,omitempty"`
    Details  string             `json:"details,omitempty"`
    Code     int                `json:"code"`
    JobUUID  uuid.UUID          `json:"jobUUID,omitempty"`   // [16]byte: never omitted
    ValidationErrors []*ValidationError `json:"validationErrors,omitempty"`
}
type ValidationError struct { FailedField, Tag, Value string } // all omitempty: failedField, tag, value
```

Field order on the wire: `error, details, code, jobUUID, validationErrors`. `jobUUID` is always
present (zero UUID `00000000-0000-0000-0000-000000000000` when unset). `Err` is never sent.

Error codes (`constants/constants.go`), with the ones actually used:

| Code | Name | Used by |
|---|---|---|
| 1000 | ErrorInvalidJobUuid | bad uuid param/query |
| 1001 | ErrorJobUuidNotFound | GET job |
| 1002 | ErrorCantCreateJob | create |
| 1003 | ErrorCantDeleteJob | delete |
| 1004 | ErrorCantPullAnyJob | pull |
| 1005 | ErrorCantPullSpecificJob | pull with `jobuuid` |
| 1006 | ErrorCantStartJob | pull with `jobuuid` |
| 1007 | ErrorCantCloneJob | clone, visibilitytimeout |
| 1008 | ErrorCantCancelJob | cancel |
| 1009 | ErrorCantSetJobAsSuccessful | succeed |
| 1010 | ErrorCantFailJob | fail |
| 1011 | ErrorCantEnqueueJob | internal only |
| 1012 | ErrorCantGetJobs | GET jobs (unreachable) |
| 1021 | ErrorInvalidParameterValue | query params |
| 1022 | ErrorCantDeserializeJson | schema: body not JSON |
| 1023 | ErrorInvalidJsonData | schema validation errors |
| 1024 | ErrorInvalidContentType | create |
| 1440 | ErrorDatabaseUnavailable | Redis init/load (startup only) |
| 1020, 1110–1114, 1220, 1330–1332 | | defined, unused |

JSON-schema validation errors (code 1023) carry one `ValidationError` per qri-io `KeyError`:
`failedField` = instance JSON pointer (`/` for the root, `/priority`…), `tag` = qri-io message,
`value` = `fmt.Sprintf("%v", invalidValue)` (Go `%v` of the decoded value: `map[a:1 b:x]`,
`[1 2 3]`, `<nil>`, `1e+06`…). Messages used by the shipped schema: `type should be <t>, got
<t>`, `"<key>" value is required`, `additional properties are not allowed`. See section 14.4.

---

## 6. JSON conventions that must be reproduced byte for byte

Go `encoding/json` (Fiber's `c.JSON`, the in-memory file, Redis values, callback payloads):

- struct fields in declaration order; map keys **sorted** (bytes); no spaces; no trailing newline.
- strings: `<`, `>`, `&` → `<`, `>`, `&`; U+2028/U+2029 → ` `/` `;
  `\b \f \n \r \t \" \\`; other control chars `\u00XX` (lowercase hex); invalid UTF-8 → U+FFFD.
- floats: shortest round-trip digits; `%f` style when 1e-6 ≤ |x| < 1e21, else exponent
  style with `e-7` / `e+21` (no leading zero in negative exponents); `1.0` prints `1`.
- nil slices and maps → `null` (`lockResources: null`, `properties: null` when absent from the
  request); empty → `[]` / `{}`.
- numbers inside `properties` are decoded as `float64` (precision lost above 2^53).

Go `encoding/json` decoding into structs (`JobRequest`, `Job`):

- key matching: exact name first, then **case-insensitive** (Unicode simple folding,
  e.g. `"TOPIC"` → `topic`); unknown keys ignored; duplicate keys: last one wins.
- `null` for a non-pointer field leaves the zero value (no error); top-level `null` body → empty
  request (accepted).
- type mismatch (string into int, 1.5 or `1e2` into int8, 200 into int8…) → error.
- trailing data after the value → error; empty body → error.
- invalid UTF-8 / lone surrogates in strings → replaced by U+FFFD (no error).

---

## 7. Job model and state machine

`job/job.go`. Wire format of `Job` (persisted) and `JobResponse` (API: same + `state`):

| Field | JSON | Go type | Notes |
|---|---|---|---|
| JobUUID | `id` | uuid (string) | UUIDv7 generated by the server |
| Topic | `topic` | string | |
| Priority | `priority` | int8 | -128..127, higher first |
| JobProperties | `properties` | `map[string]interface{}` | `null` if absent |
| History | `history` | `[{eventType:string,timestamp:int64 ms}]` | |
| LockResources | `lockResources` | `[]string` | `null` if absent |
| UserAgent, Requester, Name, SessionId, TraceId | `userAgent`, `requester`, `name`, `sessionId`, `traceId` | string | free text |
| VisibilityTimeout | `visibilityTimeout` | uint, seconds | set when the job is pulled |
| StartAfter | `startAfter` | int64, Unix ms | |
| CallbackURL | `callbackUrl` | string | |
| (JobResponse only) | `state` | string | `created delayed pending queued running succeeded failed canceled deleted hidden unknown` |

`JobRequest` = `topic, priority, properties, lockResources, userAgent, requester, name,
sessionId, traceId, startAfter (ms), delay (s), callbackUrl`. If `delay > 0`:
`startAfter = (startAfter or now) + delay*1000`.

State = function of the **last** history event (`GetStateFromEvent`, job.go:190):

| Event | State |
|---|---|
| CREATE | created |
| DELAY | delayed |
| PENDING | pending |
| ENQUEUE | queued |
| START, FAIL, RETRY | running |
| SUCCESS | succeeded |
| CANCEL | canceled |
| HIDE | hidden |
| TERMINATE | failed |
| DELETE, anything else | NoState (`unknown`) |

`previousState` = state of the second-to-last event. Completed = succeeded, canceled, failed.
State is not persisted; it is recomputed from `history` on load.

Life cycle (service/service.go):

```
create ─▶ CREATE ─┬─ startAfter > now ─▶ DELAY ──(tick 1 s, startAfter reached)──┐
                  └──────────────────────▶ PENDING ◀──────────────────────────────┘
PENDING ──(TryEnqueuePendingJobs: startAfter ≤ now and all lockResources free; locks them)──▶ ENQUEUE
ENQUEUE ──pull──▶ START (visibilityTimeout := request value)
START ─succeed─▶ SUCCESS (unlock)                           [completed]
START ─cancel──▶ CANCEL ─▶ ENQUEUE (locks kept)
START ─fail or visibility timeout─▶ FAIL ─┬─ failures ≥ maxRetry ─▶ TERMINATE (unlock) [completed]
                                          ├─ delay = 0 ─▶ ENQUEUE (locks kept)
                                          └─ delay > 0 ─▶ HIDE (unlock) ─(tick, delay elapsed)─▶ ENQUEUE (no re-lock! B10b)
any ─delete─▶ DELETE (unlock by name, B10a) then removed from memory and storage
```

- Visibility timeout: every second, running jobs with `now − lastEventTimestamp ≥
  visibilityTimeout*1000` are failed (same path as `/fail`).
- Retry delay = `ComputeDelay(failureCount)` (`retrypolicy/backoff.go`): constant `base`;
  linear `min(maxDuration, n*base)`; exponential `min(maxDuration, base*factor^n)` ms,
  `time.Duration(float)` truncation; unknown type → 0.
- Pull candidates: state queued, topic equal to `topic` (or `topic=*` for all; **empty topic
  matches only jobs with an empty topic**), sorted by priority desc then creation ms asc, first
  `numjobs`. Specific pull (`jobuuid`): ignores topic/wait, job must be queued.
- Pending scheduling: pending jobs not needing a locked resource, sorted by priority desc /
  creation asc, enqueued one by one (a job whose resources became locked by an earlier one in
  the same pass stays pending).
- Clone: copies all fields except id/history (aliases `properties` and `lockResources`), then
  normal creation.
- Retention (`jobs.retentionPolicy.enable`, every `max(interval,60)` s): completed jobs sorted
  by last event; deletes the oldest beyond `maxJobs` (if ≥ 0) and those whose last event is
  older than `maxJobAge` s (if ≥ 0).
- Restart/load: jobs reloaded from the backend; pending → pending set; hidden → retry queue
  (retry now); delayed → delayed queue; queued/running → their resources re-locked;
  created → nothing (stuck, B23).

---

## 8. Configuration

`config/config.go`, decoded with `gopkg.in/yaml.v3` (non-strict: unknown keys ignored; keys
are case-sensitive; untagged struct fields use the lowercased field name, e.g. `backend`,
`webserver.http`, `webserver.cors`). A missing key = Go zero value. yaml.v3 also accepts
YAML 1.1 booleans (`yes/no/on/off/y/n`) for bool fields, `0x`/`0o`/`_` integers, and
truncates floats into int fields.

| Key | Type | Default if absent | Unit / meaning |
|---|---|---|---|
| `environment` | string | `""` | echoed by `/api/v1/system/info` |
| `logger` | zap.Config | **required** (zero value → panic) | main logger, section 9 |
| `backend.type` | string | `""` → error | `inMemory`, `redis`, `mock` (case-sensitive) |
| `backend.logger` | zap.Config | **required** (→ error, exit 0) | backend logger |
| `backend.logVerbosity` | int | 0 | ≥ 2: debug log of each backend event |
| `backend.redis.url` | string | | go-redis URL, section 11.2 |
| `backend.redis.maxJobs` | uint | 0 → startup error | must be ≠ 0, otherwise unused |
| `backend.redis.finishedJobTTL` | uint | 0 = keep | seconds; EXPIRE on succeeded jobs only |
| `backend.inMemory.maxJobs` | uint | 0 → startup error | provider cap (see B22) |
| `backend.inMemory.writeFrequency` | int | 0 → 300 | seconds between file saves; < 0 → panic |
| `backend.inMemory.enablePersistentStorage` | bool | false | |
| `backend.inMemory.directory` | string | `""` | created (0755) if missing |
| `backend.inMemory.filename` | string | `""` | file = `directory/filename` |
| `jobs.logVerbosity` | int | 0 | > 1: info log per transition; > 2: job dumps, event logs |
| `jobs.maxAllowedJobs` | uint | 0 = unlimited | create → 503 when `count ≥ max` |
| `jobs.jsonSchema.enable` | bool | false | |
| `jobs.jsonSchema.path` | string | | relative to the **working directory** |
| `jobs.defaultVisibilityTimeout` | uint | **0** | seconds; 0 → a pulled job times out ≈1 s later |
| `jobs.maxVisibilityTimeout` | uint | **0** | seconds; 0 → any explicit `visibilitytimeout` rejected |
| `jobs.retentionPolicy.enable` | bool | false | |
| `jobs.retentionPolicy.interval` | int | 0 → 60 | seconds, minimum 60 |
| `jobs.retentionPolicy.maxJobAge` | int | 0 | seconds after the last event; < 0 = no age limit; 0 = delete at once |
| `jobs.retentionPolicy.maxJobs` | int | 0 | completed jobs kept; < 0 = no count limit; 0 = keep none |
| `jobs.retryPolicy.maxRetry` | uint | 0 | failures before TERMINATE (`count ≥ maxRetry`) |
| `jobs.retryPolicy.backoffStrategy.type` | int | 0 | 0 constant, 1 linear, 2 exponential (no names accepted) |
| `jobs.retryPolicy.backoffStrategy.base` | uint | 0 | **milliseconds** |
| `jobs.retryPolicy.backoffStrategy.factor` | float64 | 0 | exponential factor |
| `jobs.retryPolicy.backoffStrategy.maxDuration` | uint | 0 | **milliseconds**, cap for linear/exponential |
| `eventsLogger.enable` / `.filepath` | bool / string | false / `""` | JSON-lines event file (section 11.3) |
| `webserver.http.enable` | bool | false | false → no listener (process idles) |
| `webserver.http.address` | string | `""` | Go `host:port` (`:8080`, `[::1]:8080`) |
| `webserver.logs.enable` | bool | false | access log |
| `webserver.cors.enable` / `.allowOrigins` / `.allowHeaders` | bool / string / string | false / `*` / `""` | section 3 |
| `webserver.monitor.enable` | bool | false | `/monitor`, `/debug/pprof/*` |
| `webserver.metrics.enable` | bool | false | `/metrics`, `/api/v1/metrics/*`; **false crashes the server (B02)** |
| `webserver.swagger.enable` | bool | false | `/docs/*` |
| `consul.enable` | bool | false | register service at start, deregister at stop |
| `consul.server.address` / `.port` | string / int | | address/port **of the registered service** (the agent comes from `CONSUL_HTTP_ADDR`, default `127.0.0.1:8500`) |
| `consul.server.token` | string | | ACL token |
| `consul.server.replaceExistingChecks` | bool | false | |
| `consul.service.id/name/tags/namespace/partition` | | | |
| `consul.healthCheck.interval/timeout/url` | string | | Go duration strings passed to Consul as-is |
| `consul.healthCheck.enable/checkId` | | | **ignored** (the HTTP check is always registered) |

Commented keys in the templates (`bulkFlushFrequency`, `bulkMaxSize`, …) are not read.

Templates (all must work unchanged):

| File | Backend | Persistence | Schema | Address | Notes |
|---|---|---|---|---|---|
| `standard/config/config.yaml` | inMemory | `/data/minijob/jobs.json` every 30 s | on (`./config-templates/standard/config/schema.json`) | 127.0.0.1:8080 | retention 60 s / 1000 / 3600 s |
| `standard/config/config.inmemory.yaml` | inMemory | off | off | 127.0.0.1:8080 | |
| `standard/config/config.loadtest.yaml` | inMemory (maxJobs 10M) | off | off | 127.0.0.1:8080 | retention off, CORS off, visibility 30/600 s, retry base 1 ms max 2 ms |
| `docker/config/config.yaml` | inMemory | `/app/data/jobs.json` every 30 s | on (`./config/schema.json`) | 0.0.0.0:8080 | retention maxJobs 10000 |

All templates: metrics on, monitor on, swagger on, Consul off, `retryPolicy` = `maxRetry: 3,
base: 2, factor: 2, maxDuration: 60` with no `type` (→ constant **2 ms**; see B18a).
`redis.url` example: `redis://localhost:6379/3?dial_timeout=3&db=1&read_timeout=6s&max_retries=2&client_name=minijob`
(→ DB 1: the `db` query parameter overrides the path).

---

## 9. Logging (zap)

Two zap loggers built from YAML (`logger`, `backend.logger`), plus `fmt.Printf` to stdout in a
few places (Redis provider: `Listening for changes to key: ev:job:*`).

zap.Config keys used: `level` (debug/info/warn/error/dpanic/panic/fatal, upper or lower case;
`""` = info; key absent → `missing Level` error), `development`, `disableCaller`,
`disableStacktrace`, `sampling` (none in templates), `encoding` (`json`/`console`; empty →
error), `encoderConfig` (`messageKey, levelKey, timeKey, nameKey, callerKey, functionKey,
stacktraceKey, skipLineEnding, lineEnding, levelEncoder (capital|capitalColor|color|lowercase
default), timeEncoder (rfc3339nano|rfc3339|iso8601|millis|nanos|epoch default, or
{layout:…}), durationEncoder, callerEncoder, nameEncoder, consoleSeparator`), `outputPaths`,
`errorOutputPaths` (`stdout`, `stderr`, file paths), `initialFields` (map, keys sorted).

JSON line layout: `{"level":…,"<timeKey>":…,"<nameKey>":…,"<callerKey>":…,"<functionKey>":…,"<messageKey>":…,<initialFields sorted>,<fields>,"<stacktraceKey>":…}`.
iso8601 = `2006-01-02T15:04:05.000Z0700` (local time). Typical fields: `topic`, `method`,
`JobUUID`, `error`.

Startup log sequence: `Initialize logger`, `Configuration loaded`, `Server version`, `Job server
started`, `Service started`, `Start HTTP web server`.

---

## 10. Concurrency model (Go)

Goroutines:

| Goroutine | Started at | Loop |
|---|---|---|
| main | | restart loop; blocks in `HandleSignals` |
| HTTP listener | `Server.Listen` | fasthttp: one goroutine per connection |
| service loop `svc.Run` | `RunServer` | `select` on tickers and channels, spawns workers |
| backend `bp.Run` | inside `svc.Run` | in-memory/mock: save ticker; Redis: keyspace pub/sub |
| tick workers | `go svc.X()` on every tick | see below |
| callbacks | one per callback | 4 attempts, 2 s apart, 30 s timeout each |

Service loop (`service.go:1437`):

| Ticker / channel | Period / buffer | Action |
|---|---|---|
| `notifyTryEnqueuePendingJobs` | chan, cap 1000 | `go TryEnqueuePendingJobs()` per message |
| pending ticker | 1 s | sends to the channel above (from the loop itself) |
| retry ticker | 1 s | `go AttemptJobRetries()` |
| visibility ticker | 1 s | `go CheckJobsVisibilityTimeout()` (atomic "already running" flag) |
| delayed ticker | 1 s | `go CheckDelayedJobs()` (flag) |
| retention ticker | max(interval, 60) s | `go CheckJobsRetention()` (flag) |
| stats ticker | 10 s | `go UpdateJobStatistics()` (always, even with metrics off) |
| `notifChanBP` | chan, cap 1000 | backend events, only logged; `EventFatalError` stops the loop |
| `stopChan` | closed by `Stop()` | stop tickers, `bp.Stop()`, return |

Messages to `notifyTryEnqueuePendingJobs` are sent after create (while holding `svc.mu`),
succeed, cancel, terminate, visibility timeouts.

Locks:

| Lock | Protects (intended) | Notes |
|---|---|---|
| `svc.mu` (Mutex) | job transitions | taken by create, start, enqueue, retry, delayed, retention, stats; **not** by succeed/cancel/fail/delete/visibility/clone (B04) |
| `svc.mapMutex` (RWMutex) | `svc.jobs` map | `pendingJobs` and the two priority queues are touched under either lock depending on the caller (B03) |
| `svc.muPullJobs` (Mutex) | pulls | held while sleeping during long polls (B11) |
| `ResourcesManager.mu` | lock table | |
| `ServiceMetrics.mu` + `sync.Map`s + `JobMetrics.mu` | metrics | `Topics` slice unprotected (B05) |
| in-memory provider `mu` + `mapMutex` + atomic `hasChanged` | provider map (pointers shared with the service) | |
| Redis provider `mu` | every Redis command (serialized) | |

Event fan-out (`event/serviceeventnotifier.go`): synchronous, in registration order: backend
provider, metrics (if enabled), event logger (if enabled), callbacks. Called from request
goroutines, often while holding `svc.mu`. The backend does its I/O inline (Redis: one `SET`
per event).

Persistence of the in-memory backend: the provider map holds the same `*Job` pointers as the
service; every `writeFrequency` s, if `hasChanged`, the whole map is marshalled and written
with `os.WriteFile`; final save on stop.

Shutdown (signal): `HandleSignals` returns → `app.Shutdown()` (closes listeners, waits for
**all** connections to be idle, including 60 s long polls) → `apiServer.Finalize` (Consul
deregister) → `svc.Finalize` → `Stop` (close `stopChan`, wait loop: `bp.Stop` → final save) →
`eventNotifier.Shutdown()` (closes event log) → registry cleared → exit 0.

---

## 11. Data formats

### 11.1 In-memory file (`backend.inMemory.directory/filename`)

```json
{"jobs":{"<uuid>":{"id":"<uuid>","topic":"…","priority":0,"properties":{…}|null,"history":[{"eventType":"CREATE","timestamp":1735992487270},…],"lockResources":[…]|null,"userAgent":"","requester":"","name":"","sessionId":"","traceId":"","visibilityTimeout":0,"startAfter":0,"callbackUrl":""},…}}
```

- one line, keys sorted (uuids sorted as strings), Go escaping and float formatting (section 6).
- missing directory → created; missing file → `{"jobs":{}}` written at start.
- read with `json.Decoder` (first value only; trailing data ignored).
- timestamps: int64 Unix milliseconds (server clock, UTC-independent). No dates as strings.
- rewritten whole (`os.WriteFile`: create/truncate/write, mode 0644, no fsync, not atomic).

### 11.2 Redis

- key `job:<uuid>` (lowercase hyphenated), value = the job JSON above (same encoding), `SET`
  without expiry on every event; `DEL` on delete; `KEYS job:*` + `DEL` each on delete-all.
- `EXPIRE finishedJobTTL` after SUCCESS only (a later `SET` would clear it).
- load at start: `KEYS job:*` then `GET` each.
- startup: `CONFIG SET notify-keyspace-events KEA` (failure = startup error); `SUBSCRIBE
  __keyspace@0__:ev:job:*` (dead code).
- health: `PING`.
- URL (go-redis `ParseURL`): schemes `redis`, `rediss` (TLS ≥ 1.2, `skip_verify`), `unix`;
  `user:password@`; host default `localhost`, port `6379`; path `/<db>`; query `db` (overrides
  path), `protocol`, `client_name`, `max_retries`, `min_retry_backoff`, `max_retry_backoff`,
  `dial_timeout`, `read_timeout`, `write_timeout`, `pool_fifo`, `pool_size`, `pool_timeout`,
  `min_idle_conns`, `max_idle_conns`, `max_active_conns`, `max_concurrent_dials`,
  `conn_max_idle_time` (`idle_timeout`), `conn_max_lifetime` (`max_conn_age`); durations:
  plain integer = **seconds** (≤ 0 disables), else Go duration (`6s`, `500ms`); bools
  `true/1/false/0`; any other parameter → `redis: unexpected option: <names>`.

### 11.3 Event log (`eventsLogger.filepath`)

Append-only, one line per event, built by string concatenation (not escaped, B14):

```
{"timestamp": "2026-10-16T12:34:56.789", "type": "Created", "jobUUID": "<uuid>", "topic": "<topic>"}
{"timestamp": "2026-10-16T12:34:56.789", "type": "Ready"}
```

Local time, no zone. Types: `Created Delayed Pending Enqueued Started Succeeded Canceled
Failed Deleted Timeout Terminated Loaded DeletedAll UnlockedAllResources Healthcheck Metrics
EmptyPollReceived Ready Shutdown other` (`Hidden` prints `other`). Service-level events skip
`EmptyPollReceived`, `Healthcheck`, `Metrics`. Opened with create or `O_APPEND`.

### 11.4 Callback

`POST <callbackUrl>`, `Content-Type: application/json`, `User-Agent: MiniJob-Callback/1.0`,
body `{"jobId":"<uuid>","status":"<state string>","timestamp":<last event ms>,"job":<Job JSON>}`;
2xx = success; up to 4 attempts, 2 s apart, 30 s timeout; redirects followed (Go default, 10).

---

## 12. Prometheus metrics (`GET /metrics`)

Own registry (no `go_*`/`process_*` collectors). Text format 0.0.4 (gzip when accepted).

| Name | Type | Labels |
|---|---|---|
| `http_requests_total` | counter | `service="minijob"`, `status_code`, `method`, `path` (= handler metric name, e.g. `CreateJob`) |
| `http_request_duration_seconds` | histogram (1 ms … 60 s, 17 buckets) | same |
| `http_requests_in_progress_total` | gauge | `service`, `method` |
| `job_events` | counter | `event` (event names above), `topic` |
| `job_status` | gauge | `status` (`Existing Created Delayed Pending Queued Running Succeeded Failed Hidden Canceled`), `topic` |
| `job_completion_duration` | histogram (5 ms … 3600 s) | `topic`; observed on Succeeded/Deleted/Terminated with first→last event time |
| `job_active_duration_seconds` | histogram (1 s … 86400 s) | `topic`; reset every 10 s (values are ms, B15d) |
| `job_duration_percentiles` | gauge | `topic`, `status` (`active`/`completed`), `percentile` (`p5 p10 p25 p50 p75 p90 p95 p99 p100`) |
| `app_events` | counter | `event` (`EmptyPollReceived`), `topic` |

---

## 13. Docker, packaging, CI

- `Dockerfile`: golang:1.25 build (CGO off) → `alpine` + `curl`, `WORKDIR /app`, binary
  `/app/minijob`, config `/app/config/` (docker template + schema), data `/app/data`
  (created by the program), `EXPOSE 8080/tcp`, `HEALTHCHECK curl -f
  http://localhost:8080/api/v1/utils/ping` (route does not exist, B19), `CMD ["/app/minijob",
  "-config", "/app/config/config.yaml"]`, runs as root.
- `Dockerfile-goreleaser`: alpine:3.17, same layout; goreleaser only ships `config.yaml` in the
  build context (B19).
- goreleaser: linux amd64 (v2, v3), arm64, armv7; windows amd64; images
  `ghcr.io/nbigot/minijob:<ver>-linux-<arch>` + manifests.
- CI: golangci-lint v2.6, `go build`, `go test` with coverage (no `-race`, no Redis service).

---

## 14. Integrations

### 14.1 Consul (`hashicorp/consul/api`)
`PUT /v1/agent/service/register` (`ID, Name, Tags, Port, Address, Namespace, Partition, Check
{HTTP, Interval, Timeout}`, query `replace-existing-checks`, token header) at start;
`PUT /v1/agent/service/deregister/<id>` at stop. Agent address and TLS from the standard
`CONSUL_HTTP_*` environment variables.

### 14.2 Swagger
Generated by swag from handler comments (`docs/docs.go`, `docs/swagger.{json,yaml}`): title
`MiniJob API`, version `1.0`, host `127.0.0.1:8080`, license URL
`https://github.com/nbigot/minijob/blob/main/LICENSE` (to update in phase 6).

### 14.3 Monitor / pprof
Fiber monitor page and Go pprof (no Rust equivalent producing Go profiles).

### 14.4 qri-io/jsonschema v0.2.1 behavior to reproduce
- draft 2019-09 keyword set, evaluated in a fixed order (order groups: `$ref` 0, default 1,
  `properties`/`maxContains`/`minContains`/`then`/`else` 2, `additionalProperties`/
  `additionalItems` 3, `unevaluated*` 4; ties by registration order).
- type rules: a float with an integral value is `integer`; only strings that parse as a
  boolean (`"true"`, `"1"`, `"f"`...) pass `type: boolean`. (Phase 2 correction: `"5"` does
  **not** pass `type: integer` and `null` does not pass `type: string`; verified against the
  running Go server.)
- `additionalProperties: false` → a single error at the object, value = the whole object.
- `properties` iterates a Go map → several property errors come in random order.
- remote `$ref` (http/https/file) fetched lazily during the first request; draft-07
  `definitions` is ignored entirely, so `$ref: "#/definitions/x"` never resolves (two errors:
  `failed to resolve schema for ref ...` and `schema is nil`).

---

## 15. Go dependencies → Rust

| Go | Role | Rust |
|---|---|---|
| gofiber/fiber v2, valyala/fasthttp | HTTP server, router, middlewares | `hyper` 1 + `hyper-util` (own accept loop for Fiber's timeouts and graceful stop) and a small router with Fiber's matching rules (`src/web/router.rs`); CORS, healthcheck, access log, Fiber 404/405 rewritten by hand |
| gofiber/fiber monitor | `/monitor` | own handler + `sysinfo` |
| gofiber/fiber pprof | `/debug/pprof` | none (decision D5) |
| gofiber/swagger, swaggo/swag, swaggo/files | swagger UI + doc | own handler, swagger-ui assets from `swaggo/files v2.0.2` embedded (feature `swagger-ui`) |
| google/uuid | UUIDv7, parsing | `uuid` (v7, serde) + Go-compatible parser |
| gopkg.in/yaml.v3 | config | `yaml-rust2` event parser + yaml.v3 resolution and error messages (`src/config/yaml.rs`) |
| go.uber.org/zap | logs | own zap-compatible encoder (json/console) |
| encoding/json | wire format | `serde`, `serde_json` + Go formatter (escaping, floats) + Go-compatible decoder |
| qri-io/jsonschema | validation | own module porting qri-io semantics; `regex` (RE2 syntax like Go) |
| redis/go-redis v9 | Redis | `redis` (tokio, rustls, connection manager), feature `redis` |
| prometheus/client_golang | metrics | `prometheus` (no default features) |
| hashicorp/consul/api | registration | two calls with `reqwest` |
| net/http client | callbacks | `reqwest` (rustls) |
| os/signal, time, sync | runtime | `tokio`, `parking_lot`, std atomics |
| flag | CLI | small hand-written Go-flag-compatible parser |
| stretchr/testify | tests | std `assert!` |

---

## 16. Go bugs found

Severity: **C** crash / data loss, **H** wrong behavior, **M** minor. "Plan" = what the Rust
port does (subject to the decisions in section 18).

| ID | Sev | Where | Bug | Plan |
|---|---|---|---|---|
| B01 | H | `cmd/minijob/minijob.go:159` | `if !restartServer \|\| serverErr != nil { os.Exit(0) }`: every startup or runtime error (bad backend, Redis down, schema unreadable, Consul, **port in use**) exits with 0 | exit 1 on any error |
| B02 | C | `service.go:69-71,1452`, `metrics/servicemetrics.go:465,495` | `webserver.metrics.enable: false` → Prometheus vectors never initialized, but the 10 s stats tick uses them → nil dereference panic ≈10 s after start [verify]; also `p[job.Topic]` nil for unknown topics | no stats work when metrics are off; no nil paths |
| B03 | C | `service.go:428-469, 1239-1245, 262-295, 486-497` | `svc.jobs` iterated under `svc.mu` while `DeleteJob` writes it under `mapMutex` only → `concurrent map iteration and map write` fatal error; `pendingJobs` and both priority queues mutated under different locks | one lock for the whole scheduler state |
| B04 | H | `service.go:793,821,887,966,1102,428` | succeed/cancel/fail/visibility/clone/delete mutate jobs without `svc.mu`: two concurrent `/succeed` or `/fail` on one job both succeed (double transition, double unlock, double callback); history slices appended concurrently; the save goroutine and callback goroutines marshal the same `*Job` while it changes | all transitions under the state lock; persistence and callbacks use snapshots |
| B05 | M | `metrics/servicemetrics.go:451,368` | `Topics` appended without lock, check-then-store on `sync.Map` (duplicate topics in `/api/v1/topics`), `JobMetrics` read without lock, `sync.Map` reassigned while in use; mock provider uses plain bools across goroutines | metrics state behind a lock |
| B06 | H | `web/webapiserver.go:178`, `web/apijobs.go:278` | one `jsonschema.Schema` shared by all requests; qri-io mutates it on first validation (`Register`, `$ref` resolution cache, global registry) → data race; remote `$ref` downloaded during a request | schema compiled at start, immutable, shared read-only |
| B07 | C | `web/webapiserver.go:127`, `inmemoryprovider.go:230,255`, `eventlogger/eventloggerobserver.go:38,52,60`, `service.go:1160` | API shutdown stops the service (closes the backend channel, sets the event-log file to nil) while HTTP still accepts requests → `send on closed channel` / nil file → panic → crash; same after a backend fatal error; `wg.Done` runs before `running=false` → a second `Stop` can close `stopChan` twice | stop accepting first, drain, then stop the service; no panicking paths |
| B08 | C | `inmemoryprovider.go:263-279`, `dbstorage.go:85,94,95` | **records lost at stop**: `sync()` saves then clears `hasChanged`; a change in between is never saved, not even by the final save; `zap.Fatal` on a write error exits immediately; `os.WriteFile` is not atomic (crash mid-write → truncated file → next start fails); graceful stop waits for long polls before saving, so `docker stop` (10 s) can SIGKILL first | dirty generation counter; temp file + fsync + rename; final save always runs; long polls ended at shutdown |
| B09 | C | `redisprovider.go:51,128-160,485-513` | Redis write errors only logged (API says success; the job reaches Redis only if a later change to it is saved — verified with a paused Redis; each failed request waits for every retry, 18 s with the template URL); `maxJobs` not enforced; `KEYS`+`GET` at load fails the whole start if a key expires in between; `CONFIG SET` failure (managed Redis) prevents start; dead keyspace subscription pinned to DB 0; every command serialized by one mutex | decision D6 |
| B10 | H | `service/resourcesmanager.go:88,103-117`, `service.go:324-344,450` | (a) unlock by name without checking the owner: deleting a pending job, or the retention policy deleting a completed job, frees a lock held by another running job; (b) a job retried after a delay is re-enqueued without re-locking its resources; (c) duplicate names in `lockResources` → partial lock, job stuck pending forever, resource leaked | owner-checked unlock; retry goes back through the lock check; duplicates collapsed |
| B11 | H | `service.go:562-584` | long poll holds the global pull mutex while sleeping 1 s steps: one `waittimeseconds=60` request blocks every other pull for up to 60 s; new jobs seen with up to 1 s delay | no global pull lock; wake-up on enqueue |
| B13 | H | `callback/callback.go:54-104`, `service.go:148` | callbacks fire on *any* event of a completed job, including `Loaded`: every restart re-sends all callbacks; a cancel sends a callback although the job is re-queued, with a payload built later (status usually `queued`); unbounded goroutines; no URL check (SSRF) | callbacks only on the completing transition, payload snapshot at event time, bounded concurrency |
| B14 | M | `eventlogger/eventloggerobserver.go:75` | event-log JSON built by concatenation: a `"` or newline in a topic breaks the line; write error → panic | proper JSON escaping (identical output for normal topics) |
| B15 | H | `job/job.go:210`, `metrics/servicemetrics.go:180-235,427,560` | DELETE maps to NoState, so a single delete never decrements `jobsExisting`/`totalJobs` nor increments `jobsCounterDeleted`, and resource metrics of deleted jobs stay forever; `ResourcesLockedCount -= uint(inc)` with `inc < 0` adds (the Go test asserts 5 − 2 = 7); loaded jobs are not counted in `activeJobs`, so it underflows to 18446744073709551615 after a restart; `job_active_duration_seconds` is fed milliseconds; `ServiceEventJobHidden.String()` is missing (`event/serviceevents.go:129`), so hidden events are logged and counted as `other` and `jobActivity.hidden` is always 0 | correct counters (saturating), `Hidden` name, document |
| B16 | H | `web/apiresources.go:42,63,75`, `service.go:1108,1795`, `web/apijobs.go:265` | (a) resource unlock ignores the path parameter; (b) unlock errors returned as 500 `{}`; (c) visibility-timeout "not found" uses code 1007; (d) `search` is case-sensitive when lengths are equal; (e) unstable sort; (f) `Content-Type: application/json; charset=utf-8` rejected; (g) empty `topic` on pull matches only topic-less jobs (comment says "any topic") | decision D2 |
| B17 | M | `web/webserver/webserver.go:67,240` | SIGHUP handled but not registered (kills the process); SIGQUIT registered and ignored | SIGHUP = reload + restart, SIGQUIT = graceful stop (D7) |
| B18 | H | `retrypolicy/backoff.go`, templates, `config/config.go` | (a) backoff `base`/`maxDuration` are ms but templates write `2`/`60` and omit `type` → constant 2 ms retry delay; (b) visibility timeouts default to 0 when omitted; (c) negative `writeFrequency` panics; (d) missing `logger` panics; (e) invalid CORS origin panics; (f) retention interval < 60 silently raised; (g) `consul.healthCheck.enable/checkId` ignored | (a) decision D1; (c-e) clean error + exit 1; others documented |
| B19 | H | `Dockerfile:15`, `Dockerfile-goreleaser:8`, `.goreleaser.yaml` | health check calls `/api/v1/utils/ping` (404) → container always unhealthy; goreleaser image has no `schema.json` but its config enables validation → container cannot start (and exits 0) | Rust image checks `/livez`, ships the whole config directory |
| B20 | M | `job/job.go:236-255` | `Clone` aliases `properties` and `lockResources` | deep copy |
| B21 | H | `service.go:257,1442,1440` | `notifyTryEnqueuePendingJobs` (cap 1000) is sent to while holding `svc.mu` and by the service loop itself: a full buffer deadlocks everything; each message spawns a goroutine that sorts all pending jobs under the lock | coalesced wake-up (`Notify`), single scheduler task |
| B22 | M | `inmemoryprovider.go:127-138` | provider `maxJobs` reached: job exists in the API but is not persisted (only logged) | create refused with 503/1002 before any state change |
| B23 | M | `service.go:138-150` | a job saved between CREATE and PENDING reloads as `created` and is never scheduled | treat `created` like `pending` on load |
| B24 | M | `service.go:121-126` | `LoadJobs` writes `svc.jobs` under `RLock` | n/a (single lock) |
| B25 | — | `fiberprometheus/fiberprometheus.go:218` | Not a bug: verified live, the `method` label is the request method. Skip paths are set but unused (no effect: only routes with a metric name are recorded) | same behavior |
| B26 | M | Fiber `middleware/cors/cors.go` | `allowOrigins` wildcard entries: the `://*.` position is taken before trimming, so `"a, https://*.b.com"` (space after the comma) never matches subdomains | entries trimmed first |
| B27 | M | gofiber/swagger `c.Path(...)` | the 404 text for an unknown file under `/docs/` shows a garbled path (`Cannot GET sope.js` for `nope.js`, buffer reuse); the access log shows the path the handler left (`index.html`, empty for `/docs`) | real path |
| B28 | M | qri-io `keyword.go` | draft-07 `definitions` ignored: every `$ref` to it fails, so such a schema rejects every request | resolved |
| B29 | M | `web/apijobs.go:474`, `metrics/servicemetrics.go:249` | the `topic` query parameter of a pull (`c.Query`, Fiber `Immutable: false`) points into the request buffer; when the pull finds no job, that string is kept as a topic name (`/api/v1/topics/`, `/api/v1/metrics/*`) and as the `app_events` label, so later requests overwrite it (verified: `none` listed as `01a1`, the start of a job id sent afterwards) | copied strings |

Phase 3 check of the race bugs: `go test -race ./...` passes, but no test exercises the
service, the providers or the handlers concurrently. The server built with `-race` and driven
by `cmd/loadtest` (3,000 jobs, concurrency 8, `-monkey 3`) plus concurrent
succeed/fail/delete of one running job reported **99 data races**: job history and state
(`job/job.go:126,144,266,271`, B04), the priority queues (`pq/pq.go:43-89`, B03), the pending
jobs map (`service/service.go:269` against a map write, B03) and `ResourcesLockedCount`
(`metrics/servicemetrics.go:427`, B05). A double completion (B04) was not reproduced in that
run (30 attempts).

No case of "read lock taken while holding the write lock" was found: I checked every
`mapMutex` user; nested locking is `svc.mu` → `mapMutex` → (metrics / resources / provider
locks), never the reverse. The deadlock that does exist is B21 (channel send under lock).

---

## 17. Phase-1 checklist results

| Item | Finding |
|---|---|
| durations written as plain integers | No `time.Duration` field in the config (zap has none either). The same family of bug is B18a: backoff `base`/`maxDuration` are milliseconds while the templates' values (2, 60) read as seconds. go-redis URL durations are plain integers = seconds by design (documented, kept). |
| maps used by several goroutines without a lock | B03 (jobs map, pending map, priority queues), B04 (job objects), B05 (metrics). |
| read lock while holding the write lock | Not found (see end of section 16); B24 is the inverse (write under read lock); B21 is a real deadlock through a channel. |
| shared non-thread-safe library objects | B06 (qri-io schema). Checked and safe: zap loggers, `http.Client`, go-redis client, Prometheus vectors, Fiber contexts (not retained). |
| nil dereferences after a close | B07 (event-log file set to nil, closed channels, closed Redis client) and B02 (Prometheus vectors never created). |
| dates sent to MySQL as RFC 3339 | N/A: no SQL database. All dates are int64 Unix milliseconds in JSON; logs use local time. |
| transactions never rolled back | N/A: no SQL, no Redis `MULTI`. Related: state changed in memory even when the storage write fails (B09, B22). |
| records lost when the program stops | B08 (in-memory), B09 (Redis), B22, B23. |
| files rewritten without truncation | Not present: `os.WriteFile` truncates and the event log is opened with `O_APPEND`. The real issue is the non-atomic rewrite (B08). |
| exit status 0 on errors | B01. |

---

## 18. Decisions proposed (to confirm)

| # | Topic | Proposal |
|---|---|---|
| D1 | backoff units (B18a) | Keep milliseconds, as the Go code defines them; templates copied unchanged (2 ms delay); MIGRATION.md explains how to get the intended 2 s (`base: 2000, maxDuration: 60000, type: 2`) |
| D2 | API-visible bugs (B16) | Fix (a) path parameter, with `?resourcename=` still accepted; (b) unlock errors as APIError (404 unknown resource); (d) case-insensitive search; (e) stable sort with the UUID as final tie-break; (f) accept `application/json` with parameters (`; charset=utf-8`). Keep (c) code 1007 and (g) empty-topic semantics (contracts that clients may rely on) and document them |
| D3 | JSON schema | Port qri-io's semantics (messages, paths, values, type coercion) so `validationErrors` stay identical; deterministic order (sorted keys); `$ref` resolved within the schema document only (remote refs → `failed to resolve schema for ref <ref>`, the message qri-io gives when a fetch fails) |
| D4 | numbers in `properties` | Keep integers exactly (Go rounds above 2^53), format floats like Go |
| D5 | `/debug/pprof/*` | Not available (404), documented; `/monitor` reimplemented (HTML + JSON) |
| D6 | Redis | Same keys and JSON; writes ordered per job and pipelined; the request waits for the write; on a Redis error, respond as Go does but keep retrying in the background, report `/readyz` 503 and log; `CONFIG SET` failure = warning, not fatal; drop the dead keyspace subscription; `SCAN` + `MGET` at load, skipping keys that expired; enforce `maxJobs` → 503/1002 |
| D7 | signals | SIGINT/SIGTERM/SIGQUIT = graceful stop, SIGHUP = reload + restart (Unix); Ctrl+C / Ctrl+Break on Windows |
| D8 | HTTP metrics `method` label | Use the request method if phase 3 confirms Go reports `USE` |
| D9 | shutdown | Stop accepting, wake long polls (they get the normal 404/1004 answer), finish in-flight requests (10 s max), final save, exit 0 |
| D10 | client tools | `cmd/loadtest` works unchanged against the Rust server, not ported; `cmd/benchmark` (stub that calls example.com) not ported; phase 4 gets its own load generator |
| D11 | Docker | `alpine` + `curl` like Go (same paths, port, `CMD`), static musl binary, `HEALTHCHECK` on `/livez`, non-root user only if `/app/data` permissions allow it (to check: a Go-created volume is owned by root) |

---

## 19. Proposed crates and module layout (draft, to approve)

### Crates

Versions: latest releases at the start of phase 2, locked in `Cargo.lock`.

| Crate | Use | Feature |
|---|---|---|
| `tokio` (rt-multi-thread, net, time, sync, signal, fs, macros) | runtime | core |
| `hyper` 1, `hyper-util`, `http`, `http-body-util`, `bytes`, `futures-util` | HTTP server; own router (see below) | core |
| `serde`, `serde_json` | JSON (+ own Go formatter/decoder) | core |
| `yaml-rust2` | YAML events (raw scalars, styles, lines) | core |
| `uuid` (v7, serde) | ids | core |
| `chrono` | local time in logs and the event file | core |
| `parking_lot` | locks | core |
| `thiserror` | error types | core |
| `regex` | JSON-schema `pattern`, `patternProperties` | core |
| `prometheus` (no default features) | `/metrics` | core |
| `reqwest` (rustls, no default features) | callbacks, Consul | core |
| `sysinfo` | `/monitor`, `/api/v1/system/resources` | core |
| `percent-encoding`, `form_urlencoded` | raw path parameters, first-value query parsing | core |
| `redis` (tokio-comp, tokio-rustls-comp, connection-manager) | Redis backend | `redis` (default) |
| (embedded files) swagger-ui from `swaggo/files v2.0.2`, Apache-2.0 | `/docs/*` | `swagger-ui` (default) |
| `mimalloc` | global allocator of the Docker image (musl) | `mimalloc` (on in the Docker image since phase 4, see bench/README.md) |
| dev: `tempfile`, `reqwest` | integration tests | |

Changes to the default stack: no `sqlx` (no SQL database), no `jaq` (no jq); `redis`,
`reqwest`, `prometheus`, `sysinfo`, `regex`, YAML added. No `clap`: Go's `flag` syntax and
`-h` output are reproduced by hand. No `tracing`: zap's line format is reproduced by a small
logger (one less layer to configure).

Changed during phase 2:

- **No `axum`.** Fiber tries routes in registration order (first match wins, whatever the
  method) and answers 405 with an `Allow` header only for real routes, not for middlewares.
  axum's router prefers static segments: `GET /api/v1/job/pull` would answer 405 instead of
  Go's 400 "invalid job uuid", and its 404/405 bodies differ. A 150-line router reproduces
  Fiber exactly; hyper serves it directly.
- **`yaml-rust2` instead of a serde YAML crate**, to keep the raw text, style and line of each
  scalar: yaml.v3 resolves `017` as octal, accepts `yes`/`on` for bools, and its error
  messages quote the raw value and line.
- **rustls with the `ring` provider** (reqwest `rustls-no-provider`): the default `aws-lc-rs`
  needs cmake and a C toolchain, which complicates the musl and armv7 cross builds.
- `serde_json` with `raw_value`, to quote `enum` values exactly as written in the schema.

Release profile: `lto = "thin"`, `codegen-units = 1`, `strip = true`. MSRV: 1.95 (required by
`sysinfo` 0.39), verified with a 1.95.0 toolchain and by a CI job.


### Runtime design

- One `parking_lot::Mutex<SchedulerState>`: jobs (`HashMap<Uuid, Arc<Job>>`, copy-on-write),
  pending set, delayed and retry queues (`BTreeSet` keyed by time + sequence, removable by id),
  resource-lock table. Every transition happens under it; no I/O while it is held.
- Observers are called after each transition with a snapshot (`Arc<Job>`): metrics
  (in-process, cheap), event file (channel → writer task, flushed when the channel is empty),
  callbacks (bounded task pool), storage.
- Storage: in-memory = periodic snapshot (map of `Arc`s cloned under the lock, serialized
  outside), dirty generation counter, temp file + fsync + rename. Redis = ordered write queue,
  pipelined, the request awaits its write.
- One scheduler task (1 s tick + `Notify` wake-ups) replaces the per-tick goroutines; long
  polls wait on a `Notify` with a deadline, no global pull lock.
- Server: own accept loop (HTTP/1 only) with Fiber's 10 s read/write/idle timeouts, connection
  limit, body limit, graceful stop; panics in handlers are caught (500) instead of crashing.

### Layout

```
minijob-rs/
├── Cargo.toml  Cargo.lock  README.md  LICENSE  MIGRATION.md  PORTING-NOTES.md
├── Dockerfile  .dockerignore  .github/workflows/ci.yml
├── config-templates/            copy of the Go templates (unchanged)
├── docs/                        swagger.json / swagger.yaml (served as /docs/doc.json), API notes
├── assets/swagger-ui/           swagger-ui dist + its LICENSE   (feature swagger-ui)
├── src/
│   ├── main.rs                  restart loop, exit codes
│   ├── cli.rs                   Go-flag-compatible parser, -version, usage
│   ├── lib.rs
│   ├── config/   mod.rs  yaml.rs (yaml.v3 compat)  zap.rs
│   ├── log/      mod.rs  json.rs  console.rs  sink.rs          zap-compatible logger
│   ├── gojson/   encode.rs (escaping, floats)  decode.rs (case-insensitive fields, nulls)  fmt_v.rs (%v)
│   ├── model/    job.rs  state.rs  request.rs  uuid.rs (Go-compatible parsing)
│   ├── service/  mod.rs  state.rs  queue.rs  resources.rs  retry.rs  scheduler.rs  query.rs (GET /jobs)
│   ├── events/   mod.rs (event types, fan-out)  eventlog.rs  callback.rs
│   ├── metrics/  mod.rs  service_metrics.rs  prometheus.rs  percentiles.rs  http.rs
│   ├── storage/  mod.rs (trait)  inmemory.rs  file.rs  redis.rs  redis_url.rs  mock.rs
│   ├── schema/   mod.rs  keywords/*.rs  formats.rs                qri-io-compatible validator
│   ├── web/      server.rs  router.rs  error.rs (APIError)  results.rs  query.rs
│   │             handlers/{jobs,jobs_list,resources,admin,system,utils,metrics}.rs
│   │             middleware/{cors,access_log,http_metrics,healthcheck,fallback}.rs
│   │             monitor.rs  swagger.rs
│   └── consul.rs
├── tests/
│   ├── common/mod.rs            start the binary on a temp copy of a template, HTTP helpers
│   ├── template_standard.rs  template_inmemory.rs  template_loadtest.rs  template_docker.rs
│   ├── storage_inmemory_file.rs (reads Go-written files, restart)  storage_mock.rs
│   ├── storage_redis.rs         runs only when MINIJOB_TEST_REDIS_URL is set
│   └── security.rs              CORS, limits, path handling, escaping, unauthenticated admin routes
├── compare/                     phase 3: differential client (Go vs Rust) + scenarios
└── bench/                       phase 4: load generator + scripts
```

Unit tests ported from Go live next to the code (`job`, `pq` → `queue`, `retrypolicy`,
`percentiles`, `servicemetrics`, `prometheusmetrics`, `apihelpers`, `log`); Go tests that assert
a Go bug (e.g. `TestServiceMetrics_UpdateResourcesLockedCountMetric` expects 5 − 2 = 7) are
ported with the corrected expectation and a comment.

Quality gates: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings` with default
features, `--no-default-features` and `--all-features`; `cargo test --locked`; Docker build.
CI runs the same with a `redis` service container.
