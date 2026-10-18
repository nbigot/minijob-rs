//! Redis URL parsing with the rules of go-redis v9 `ParseURL`.

use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    Tcp { host: String, port: u16 },
    Unix(String),
}

/// A timeout: unset (library default), disabled (`<= 0`), or a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Timeout {
    #[default]
    Default,
    Disabled,
    Value(Duration),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RedisOptions {
    pub address: Option<Address>,
    pub tls: bool,
    pub skip_verify: bool,
    pub username: String,
    pub password: String,
    pub db: i64,
    pub protocol: i64,
    pub client_name: String,
    pub max_retries: i64,
    pub dial_timeout: Timeout,
    pub read_timeout: Timeout,
    pub write_timeout: Timeout,
}

impl RedisOptions {
    /// go-redis defaults: dial 5 s, read 3 s.
    pub fn dial(&self) -> Option<Duration> {
        match self.dial_timeout {
            Timeout::Default => Some(Duration::from_secs(5)),
            Timeout::Disabled => None,
            Timeout::Value(d) => Some(d),
        }
    }

    pub fn read(&self) -> Option<Duration> {
        match self.read_timeout {
            Timeout::Default => Some(Duration::from_secs(3)),
            Timeout::Disabled => None,
            Timeout::Value(d) => Some(d),
        }
    }

    /// go-redis: 0 → 3 retries, -1 → none.
    pub fn retries(&self) -> usize {
        match self.max_retries {
            0 => 3,
            n if n < 0 => 0,
            n => n as usize,
        }
    }
}

/// Go `time.ParseDuration`.
pub fn parse_go_duration(s: &str) -> Result<Duration, String> {
    let err = || format!("time: invalid duration {s:?}");
    let (neg, mut rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if rest == "0" {
        return Ok(Duration::ZERO);
    }
    if rest.is_empty() {
        return Err(err());
    }
    let mut total: f64 = 0.0;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let num = &rest[..num_end];
        if num.is_empty() || num == "." {
            return Err(err());
        }
        let value: f64 = num.parse().map_err(|_| err())?;
        rest = &rest[num_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        rest = &rest[unit_end..];
        let nanos = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            "" => return Err(format!("time: missing unit in duration {s:?}")),
            u => return Err(format!("time: unknown unit {u:?} in duration {s:?}")),
        };
        total += value * nanos;
    }
    if neg {
        // Negative durations disable go-redis timeouts; callers only check the sign.
        return Ok(Duration::ZERO);
    }
    Ok(Duration::from_nanos(total as u64))
}

struct Query {
    pairs: Vec<(String, String)>,
    err: Option<String>,
}

impl Query {
    fn has(&self, name: &str) -> bool {
        self.pairs.iter().any(|(k, _)| k == name)
    }

    /// First value, and removes the parameter (go-redis `queryOptions.string`).
    fn string(&mut self, name: &str) -> String {
        let first = self
            .pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone());
        self.pairs.retain(|(k, _)| k != name);
        first.unwrap_or_default()
    }

    fn int(&mut self, name: &str) -> i64 {
        let s = self.string(name);
        if s.is_empty() {
            return 0;
        }
        match s.parse::<i64>() {
            Ok(i) => i,
            Err(_) => {
                self.err.get_or_insert(format!(
                    "redis: invalid {name} number: strconv.Atoi: parsing {s:?}: invalid syntax"
                ));
                0
            }
        }
    }

    fn duration(&mut self, name: &str) -> Timeout {
        let s = self.string(name);
        if s.is_empty() {
            return Timeout::Default;
        }
        if let Ok(i) = s.parse::<i64>() {
            return if i <= 0 {
                Timeout::Disabled
            } else {
                Timeout::Value(Duration::from_secs(i as u64))
            };
        }
        match parse_go_duration(&s) {
            Ok(d) if s.starts_with('-') || d.is_zero() => Timeout::Disabled,
            Ok(d) => Timeout::Value(d),
            Err(e) => {
                self.err
                    .get_or_insert(format!("redis: invalid {name} duration: {e}"));
                Timeout::Default
            }
        }
    }

    fn boolean(&mut self, name: &str) -> bool {
        let s = self.string(name);
        match s.as_str() {
            "true" | "1" => true,
            "false" | "0" | "" => false,
            _ => {
                self.err.get_or_insert(format!(
                    "redis: invalid {name} boolean: expected true/false/1/0 or an empty string, got {s:?}"
                ));
                false
            }
        }
    }
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Parses a Redis URL (`redis://`, `rediss://`, `unix://`) like go-redis.
pub fn parse(url: &str) -> Result<RedisOptions, String> {
    let u = url::Url::parse(url).map_err(|e| format!("parse {url:?}: {e}"))?;
    let mut o = RedisOptions::default();
    match u.scheme() {
        "redis" | "rediss" => {
            let host = match u.host_str() {
                Some(h) if !h.is_empty() => h.trim_start_matches('[').trim_end_matches(']'),
                _ => "localhost",
            };
            o.address = Some(Address::Tcp {
                host: host.to_string(),
                port: u.port().unwrap_or(6379),
            });
            let path_parts: Vec<&str> = u.path().split('/').filter(|p| !p.is_empty()).collect();
            match path_parts.as_slice() {
                [] => o.db = 0,
                [db] => {
                    o.db = db
                        .parse()
                        .map_err(|_| format!("redis: invalid database number: {db:?}"))?;
                }
                _ => return Err(format!("redis: invalid URL path: {}", u.path())),
            }
            o.tls = u.scheme() == "rediss";
        }
        "unix" => {
            if u.path().trim().is_empty() {
                return Err("redis: empty unix socket path".to_string());
            }
            o.address = Some(Address::Unix(u.path().to_string()));
        }
        other => return Err(format!("redis: invalid URL scheme: {other}")),
    }
    o.username = decode(u.username());
    o.password = u.password().map(decode).unwrap_or_default();
    let mut q = Query {
        pairs: u.query_pairs().into_owned().collect(),
        err: None,
    };
    let db = q.string("db");
    if !db.is_empty() {
        o.db = db.parse().map_err(|_| {
            format!("redis: invalid database number: strconv.Atoi: parsing {db:?}: invalid syntax")
        })?;
    }
    o.protocol = q.int("protocol");
    o.client_name = q.string("client_name");
    o.max_retries = q.int("max_retries");
    q.duration("min_retry_backoff");
    q.duration("max_retry_backoff");
    o.dial_timeout = q.duration("dial_timeout");
    o.read_timeout = q.duration("read_timeout");
    o.write_timeout = q.duration("write_timeout");
    q.boolean("pool_fifo");
    q.int("pool_size");
    q.duration("pool_timeout");
    q.int("min_idle_conns");
    q.int("max_idle_conns");
    q.int("max_active_conns");
    q.int("max_concurrent_dials");
    if q.has("conn_max_idle_time") {
        q.duration("conn_max_idle_time");
    } else {
        q.duration("idle_timeout");
    }
    if q.has("conn_max_lifetime") {
        q.duration("conn_max_lifetime");
    } else {
        q.duration("max_conn_age");
    }
    if let Some(e) = q.err.take() {
        return Err(e);
    }
    if o.tls && q.has("skip_verify") {
        o.skip_verify = q.boolean("skip_verify");
        if let Some(e) = q.err.take() {
            return Err(e);
        }
    }
    if !q.pairs.is_empty() {
        let mut names: Vec<String> = q.pairs.iter().map(|(k, _)| k.clone()).collect();
        names.sort();
        names.dedup();
        return Err(format!("redis: unexpected option: {}", names.join(", ")));
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_url() {
        let o = parse("redis://localhost:6379/3?dial_timeout=3&db=1&read_timeout=6s&max_retries=2&client_name=minijob").unwrap();
        assert_eq!(
            o.address,
            Some(Address::Tcp {
                host: "localhost".into(),
                port: 6379
            })
        );
        assert_eq!(o.db, 1);
        assert_eq!(o.dial(), Some(Duration::from_secs(3)));
        assert_eq!(o.read(), Some(Duration::from_secs(6)));
        assert_eq!(o.retries(), 2);
        assert_eq!(o.client_name, "minijob");
    }

    #[test]
    fn variants_and_errors() {
        let o = parse("rediss://user:p%40ss@[::1]/2?skip_verify=true&read_timeout=-1").unwrap();
        assert!(o.tls && o.skip_verify);
        assert_eq!(o.username, "user");
        assert_eq!(o.password, "p@ss");
        assert_eq!(o.db, 2);
        assert_eq!(o.read(), None);
        assert_eq!(o.dial(), Some(Duration::from_secs(5)));
        let o = parse("redis://").unwrap();
        assert_eq!(
            o.address,
            Some(Address::Tcp {
                host: "localhost".into(),
                port: 6379
            })
        );
        assert_eq!(
            parse("http://x").unwrap_err(),
            "redis: invalid URL scheme: http"
        );
        assert_eq!(
            parse("redis://h/a/b").unwrap_err(),
            "redis: invalid URL path: /a/b"
        );
        assert_eq!(
            parse("redis://h/x").unwrap_err(),
            "redis: invalid database number: \"x\""
        );
        assert_eq!(
            parse("redis://h/?foo=1&bar=2").unwrap_err(),
            "redis: unexpected option: bar, foo"
        );
        assert!(
            parse("redis://h/?dial_timeout=abc")
                .unwrap_err()
                .starts_with("redis: invalid dial_timeout duration: time: ")
        );
        assert!(parse("unix://").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn go_durations() {
        assert_eq!(parse_go_duration("6s").unwrap(), Duration::from_secs(6));
        assert_eq!(
            parse_go_duration("1h30m").unwrap(),
            Duration::from_secs(5400)
        );
        assert_eq!(
            parse_go_duration("1.5s").unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(
            parse_go_duration("250ms").unwrap(),
            Duration::from_millis(250)
        );
        assert!(parse_go_duration("10").is_err());
        assert!(parse_go_duration("1d").is_err());
    }
}
