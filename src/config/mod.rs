//! Configuration file (`-config`), same keys, defaults and units as the Go version
//! (`config/config.go`). A missing key keeps Go's zero value.

pub mod yaml;
pub mod zap;

use self::yaml::{Decoder, Node};
pub use self::zap::ZapConfig;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub environment: String,
    pub backend: BackendConfig,
    pub jobs: JobsConfig,
    pub logger: ZapConfig,
    pub events_logger: EventsLoggerConfig,
    pub webserver: WebServerConfig,
    pub consul: ConsulConfig,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackendConfig {
    /// `inMemory`, `redis` or `mock`.
    pub kind: String,
    pub logger: ZapConfig,
    pub log_verbosity: i64,
    pub redis: RedisConfig,
    pub in_memory: InMemoryConfig,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RedisConfig {
    pub url: String,
    pub max_jobs: u64,
    /// Seconds, 0 = keep forever.
    pub finished_job_ttl: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InMemoryConfig {
    pub max_jobs: u64,
    /// Seconds between saves, 0 = 300.
    pub write_frequency: i64,
    pub enable_persistent_storage: bool,
    pub directory: String,
    pub filename: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobsConfig {
    pub log_verbosity: i64,
    /// 0 = no limit.
    pub max_allowed_jobs: u64,
    pub json_schema: JsonSchemaConfig,
    /// Seconds.
    pub default_visibility_timeout: u64,
    /// Seconds.
    pub max_visibility_timeout: u64,
    pub retention_policy: RetentionPolicyConfig,
    pub retry_policy: RetryPolicyConfig,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JsonSchemaConfig {
    pub enable: bool,
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetentionPolicyConfig {
    pub enable: bool,
    /// Seconds, at least 60.
    pub interval: i64,
    /// Seconds after the last event, negative = no age limit.
    pub max_job_age: i64,
    /// Completed jobs kept, negative = no count limit.
    pub max_jobs: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetryPolicyConfig {
    pub max_retry: u64,
    pub backoff_strategy: BackoffStrategy,
}

/// Backoff between retries. `base` and `max_duration` are milliseconds, as in the Go code.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackoffStrategy {
    /// 0 constant, 1 linear, 2 exponential, other = no delay.
    pub kind: i64,
    pub base: u64,
    pub factor: f64,
    pub max_duration: u64,
}

impl BackoffStrategy {
    /// Delay before the next retry after `failures` failures (`ComputeDelay`), in milliseconds.
    pub fn delay_ms(&self, failures: u64) -> u64 {
        match self.kind {
            0 => self.base,
            1 => self.max_duration.min(failures.wrapping_mul(self.base)),
            2 => {
                let v = (self.max_duration as f64)
                    .min(self.base as f64 * self.factor.powf(failures as f64));
                // Go converts the float to a Duration (truncation) after multiplying.
                go_float_to_u64(v)
            }
            _ => 0,
        }
    }
}

/// Go `time.Duration(f) * time.Millisecond` truncation of a non-negative float.
fn go_float_to_u64(v: f64) -> u64 {
    if v.is_nan() || v <= 0.0 {
        0
    } else if v >= u64::MAX as f64 {
        u64::MAX
    } else {
        v as u64
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventsLoggerConfig {
    pub enable: bool,
    pub file_path: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebServerConfig {
    pub http: HttpConfig,
    pub logs: Toggle,
    pub cors: CorsConfig,
    pub monitor: Toggle,
    pub metrics: Toggle,
    pub swagger: Toggle,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HttpConfig {
    pub enable: bool,
    pub address: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Toggle {
    pub enable: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CorsConfig {
    pub enable: bool,
    pub allow_origins: String,
    pub allow_headers: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConsulConfig {
    pub enable: bool,
    pub server: ConsulServer,
    pub service: ConsulService,
    pub health_check: ConsulHealthCheck,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConsulServer {
    pub address: String,
    pub port: i64,
    pub token: String,
    pub replace_existing_checks: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConsulService {
    pub id: String,
    pub name: String,
    pub tags: Option<Vec<String>>,
    pub namespace: String,
    pub partition: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConsulHealthCheck {
    pub enable: bool,
    pub check_id: String,
    pub interval: String,
    pub timeout: String,
    pub url: String,
}

/// Go type names, used in yaml.v3 error messages.
const ANON: &str = "struct { ... }";

fn toggle(dec: &mut Decoder, node: &Node, out: &mut Toggle) {
    dec.mapping(node, ANON, |d, k, v| match k {
        "enable" => {
            d.bool(v, &mut out.enable);
            true
        }
        _ => false,
    });
}

fn decode_backend(dec: &mut Decoder, node: &Node, out: &mut BackendConfig) {
    dec.mapping(node, ANON, |d, k, v| {
        match k {
            "type" => d.string(v, &mut out.kind),
            "logger" => zap::decode(d, v, &mut out.logger),
            "logVerbosity" => d.int(v, &mut out.log_verbosity, "int"),
            "redis" => d.mapping(v, ANON, |d, k, v| {
                match k {
                    "url" => d.string(v, &mut out.redis.url),
                    "maxJobs" => d.uint(v, &mut out.redis.max_jobs, "uint"),
                    "finishedJobTTL" => d.uint(v, &mut out.redis.finished_job_ttl, "uint"),
                    _ => return false,
                }
                true
            }),
            "inMemory" => d.mapping(v, ANON, |d, k, v| {
                let m = &mut out.in_memory;
                match k {
                    "maxJobs" => d.uint(v, &mut m.max_jobs, "uint"),
                    "writeFrequency" => d.int(v, &mut m.write_frequency, "int"),
                    "enablePersistentStorage" => d.bool(v, &mut m.enable_persistent_storage),
                    "directory" => d.string(v, &mut m.directory),
                    "filename" => d.string(v, &mut m.filename),
                    _ => return false,
                }
                true
            }),
            _ => return false,
        }
        true
    });
}

fn decode_jobs(dec: &mut Decoder, node: &Node, out: &mut JobsConfig) {
    dec.mapping(node, ANON, |d, k, v| {
        match k {
            "logVerbosity" => d.int(v, &mut out.log_verbosity, "int"),
            "maxAllowedJobs" => d.uint(v, &mut out.max_allowed_jobs, "uint"),
            "jsonSchema" => d.mapping(v, ANON, |d, k, v| {
                match k {
                    "enable" => d.bool(v, &mut out.json_schema.enable),
                    "path" => d.string(v, &mut out.json_schema.path),
                    _ => return false,
                }
                true
            }),
            "defaultVisibilityTimeout" => d.uint(v, &mut out.default_visibility_timeout, "uint"),
            "maxVisibilityTimeout" => d.uint(v, &mut out.max_visibility_timeout, "uint"),
            "retentionPolicy" => d.mapping(v, ANON, |d, k, v| {
                let r = &mut out.retention_policy;
                match k {
                    "enable" => d.bool(v, &mut r.enable),
                    "interval" => d.int(v, &mut r.interval, "int"),
                    "maxJobAge" => d.int(v, &mut r.max_job_age, "int"),
                    "maxJobs" => d.int(v, &mut r.max_jobs, "int"),
                    _ => return false,
                }
                true
            }),
            "retryPolicy" => d.mapping(v, "retrypolicy.RetryPolicy", |d, k, v| {
                let r = &mut out.retry_policy;
                match k {
                    "maxRetry" => d.uint(v, &mut r.max_retry, "uint"),
                    "backoffStrategy" => d.mapping(v, "retrypolicy.BackoffStrategy", |d, k, v| {
                        let b = &mut r.backoff_strategy;
                        match k {
                            "type" => d.int(v, &mut b.kind, "retrypolicy.BackoffStrategyType"),
                            "base" => d.uint(v, &mut b.base, "uint"),
                            "factor" => d.float(v, &mut b.factor),
                            "maxDuration" => d.uint(v, &mut b.max_duration, "uint"),
                            _ => return false,
                        }
                        true
                    }),
                    _ => return false,
                }
                true
            }),
            _ => return false,
        }
        true
    });
}

fn decode_webserver(dec: &mut Decoder, node: &Node, out: &mut WebServerConfig) {
    dec.mapping(node, "config.WebServerConfig", |d, k, v| {
        match k {
            "http" => d.mapping(v, ANON, |d, k, v| {
                match k {
                    "enable" => d.bool(v, &mut out.http.enable),
                    "address" => d.string(v, &mut out.http.address),
                    _ => return false,
                }
                true
            }),
            "logs" => toggle(d, v, &mut out.logs),
            "cors" => d.mapping(v, ANON, |d, k, v| {
                match k {
                    "enable" => d.bool(v, &mut out.cors.enable),
                    "allowOrigins" => d.string(v, &mut out.cors.allow_origins),
                    "allowHeaders" => d.string(v, &mut out.cors.allow_headers),
                    _ => return false,
                }
                true
            }),
            "monitor" => toggle(d, v, &mut out.monitor),
            "metrics" => toggle(d, v, &mut out.metrics),
            "swagger" => toggle(d, v, &mut out.swagger),
            _ => return false,
        }
        true
    });
}

fn decode_consul(dec: &mut Decoder, node: &Node, out: &mut ConsulConfig) {
    dec.mapping(node, "config.ConsulConfig", |d, k, v| {
        match k {
            "enable" => d.bool(v, &mut out.enable),
            "server" => d.mapping(v, ANON, |d, k, v| {
                let s = &mut out.server;
                match k {
                    "address" => d.string(v, &mut s.address),
                    "port" => d.int(v, &mut s.port, "int"),
                    "token" => d.string(v, &mut s.token),
                    "replaceExistingChecks" => d.bool(v, &mut s.replace_existing_checks),
                    _ => return false,
                }
                true
            }),
            "service" => d.mapping(v, ANON, |d, k, v| {
                let s = &mut out.service;
                match k {
                    "id" => d.string(v, &mut s.id),
                    "name" => d.string(v, &mut s.name),
                    "tags" => d.string_list(v, &mut s.tags),
                    "namespace" => d.string(v, &mut s.namespace),
                    "partition" => d.string(v, &mut s.partition),
                    _ => return false,
                }
                true
            }),
            "healthCheck" => d.mapping(v, ANON, |d, k, v| {
                let h = &mut out.health_check;
                match k {
                    "enable" => d.bool(v, &mut h.enable),
                    "checkId" => d.string(v, &mut h.check_id),
                    "interval" => d.string(v, &mut h.interval),
                    "timeout" => d.string(v, &mut h.timeout),
                    "url" => d.string(v, &mut h.url),
                    _ => return false,
                }
                true
            }),
            _ => return false,
        }
        true
    });
}

/// Decodes the configuration from YAML text, with yaml.v3's behavior and messages.
pub fn parse(text: &str) -> Result<Config, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(root) = yaml::parse_first_document(text)? else {
        return Err("EOF".to_string());
    };
    let mut cfg = Config::default();
    let mut dec = Decoder::new();
    dec.mapping(&root, "config.Config", |d, k, v| {
        match k {
            "environment" => d.string(v, &mut cfg.environment),
            "backend" => decode_backend(d, v, &mut cfg.backend),
            "jobs" => decode_jobs(d, v, &mut cfg.jobs),
            "logger" => zap::decode(d, v, &mut cfg.logger),
            "eventsLogger" => d.mapping(v, ANON, |d, k, v| {
                match k {
                    "enable" => d.bool(v, &mut cfg.events_logger.enable),
                    "filepath" => d.string(v, &mut cfg.events_logger.file_path),
                    _ => return false,
                }
                true
            }),
            "webserver" => decode_webserver(d, v, &mut cfg.webserver),
            "consul" => decode_consul(d, v, &mut cfg.consul),
            _ => return false,
        }
        true
    });
    dec.finish()?;
    Ok(cfg)
}

/// Go's message for an OS error: `open <path>: <description>` without Rust's `(os error N)`.
pub fn go_open_error(path: &str, err: &std::io::Error) -> String {
    go_path_error("open", path, err)
}

/// A Go `*fs.PathError` message: `<op> <path>: <description>` (`mkdir`, `open`, `write`...).
pub fn go_path_error(op: &str, path: &str, err: &std::io::Error) -> String {
    let mut msg = err.to_string();
    if let Some(i) = msg.find(" (os error ") {
        msg.truncate(i);
    }
    if cfg!(unix) {
        // Go prints errno strings in lowercase ("no such file or directory").
        let mut c = msg.chars();
        if let Some(first) = c.next() {
            msg = first.to_lowercase().collect::<String>() + c.as_str();
        }
    }
    format!("{op} {path}: {msg}")
}

/// Loads the configuration file (`config.LoadConfig`). The error text is printed by `main`.
pub fn load(path: &str) -> Result<Config, String> {
    let bytes = std::fs::read(path).map_err(|e| {
        format!(
            "error while opening configuration file {path} : {}",
            go_open_error(path, &e)
        )
    })?;
    let parse_err = |e: String| format!("error while parsing configuration file {path} : {e}");
    let text = String::from_utf8(bytes)
        .map_err(|_| parse_err("yaml: invalid leading UTF-8 octet".to_string()))?;
    parse(&text).map_err(parse_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(name: &str) -> Config {
        let path = format!("{}/config-templates/{name}", env!("CARGO_MANIFEST_DIR"));
        load(&path).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn standard_template() {
        let c = template("standard/config/config.yaml");
        assert_eq!(c.environment, "development");
        assert_eq!(c.backend.kind, "inMemory");
        assert_eq!(c.backend.log_verbosity, 3);
        assert_eq!(c.backend.redis.max_jobs, 100000);
        assert_eq!(c.backend.redis.finished_job_ttl, 86400);
        assert_eq!(c.backend.in_memory.write_frequency, 30);
        assert!(c.backend.in_memory.enable_persistent_storage);
        assert_eq!(c.backend.in_memory.directory, "/data/minijob");
        assert_eq!(
            c.jobs.json_schema.path,
            "./config-templates/standard/config/schema.json"
        );
        assert_eq!(c.jobs.retention_policy.max_jobs, 1000);
        assert_eq!(c.jobs.retry_policy.max_retry, 3);
        assert_eq!(c.jobs.retry_policy.backoff_strategy.base, 2);
        assert_eq!(c.jobs.retry_policy.backoff_strategy.kind, 0);
        assert_eq!(c.jobs.default_visibility_timeout, 60);
        assert_eq!(c.jobs.max_visibility_timeout, 86400);
        assert_eq!(c.webserver.http.address, "127.0.0.1:8080");
        assert!(c.webserver.cors.enable);
        assert_eq!(c.webserver.cors.allow_origins, "*");
        assert!(c.webserver.metrics.enable);
        assert_eq!(c.consul.service.tags, Some(vec!["jobs".to_string()]));
        assert_eq!(c.consul.health_check.interval, "10s");
        assert_eq!(c.logger.encoding, "json");
    }

    #[test]
    fn all_templates_load() {
        for t in [
            "standard/config/config.yaml",
            "standard/config/config.inmemory.yaml",
            "standard/config/config.loadtest.yaml",
            "docker/config/config.yaml",
        ] {
            template(t);
        }
        let c = template("standard/config/config.loadtest.yaml");
        assert_eq!(c.backend.in_memory.max_jobs, 10_000_000);
        assert_eq!(c.backend.in_memory.write_frequency, 0);
        assert!(!c.jobs.json_schema.enable);
        assert_eq!(c.events_logger.file_path, "events.log");
    }

    #[test]
    fn errors_like_go() {
        assert_eq!(parse("").unwrap_err(), "EOF");
        let e = parse("jobs:\n  maxAllowedJobs: abc\nwebserver:\n  http:\n    enable: maybe\n")
            .unwrap_err();
        assert_eq!(
            e,
            "yaml: unmarshal errors:\n  line 2: cannot unmarshal !!str `abc` into uint\n  line 5: cannot unmarshal !!str `maybe` into bool"
        );
        let c = parse("webserver:\n  http:\n    enable: yes\nunknown: 1\n").unwrap();
        assert!(c.webserver.http.enable);
        assert!(load("does-not-exist.yaml")
            .unwrap_err()
            .starts_with("error while opening configuration file does-not-exist.yaml : open does-not-exist.yaml: "));
    }

    // Port of retrypolicy/backoff_test.go (delays in milliseconds).
    #[test]
    fn compute_delay() {
        let b = |kind, base, factor, max| BackoffStrategy {
            kind,
            base,
            factor,
            max_duration: max,
        };
        assert_eq!(b(0, 2000, 0.0, 0).delay_ms(3), 2000);
        assert_eq!(b(1, 2000, 0.0, 99000).delay_ms(3), 6000);
        assert_eq!(b(1, 2000, 0.0, 5000).delay_ms(3), 5000);
        assert_eq!(b(2, 2000, 2.0, 99000).delay_ms(3), 16000);
        assert_eq!(b(2, 1000, 1.4, 99000).delay_ms(4), 3841);
        assert_eq!(b(2, 2000, 2.0, 1000).delay_ms(3), 1000);
        assert_eq!(b(99, 2000, 0.0, 0).delay_ms(3), 0);
    }
}
