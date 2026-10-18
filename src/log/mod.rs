//! Logger producing the same lines as the Go server's zap loggers, configured by the same
//! `logger` / `backend.logger` sections.

pub mod gotime;

use crate::config::yaml::AnyValue;
pub use crate::config::zap::Level;
use crate::config::zap::{CallerEncoder, LevelEncoder, TimeEncoder, ZapConfig};
use chrono::{DateTime, FixedOffset};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::panic::Location;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A log field value.
#[derive(Clone, Debug)]
pub enum FieldValue {
    Str(String),
    Int(i64),
    Uint(u64),
    Float(f64),
    Bool(bool),
    /// Already encoded JSON (Go `json.Marshal` of a reflected value).
    Json(String),
}

#[derive(Clone, Debug)]
pub struct Field {
    pub key: String,
    pub value: FieldValue,
}

/// `zap.String`.
pub fn s(key: &str, value: impl Into<String>) -> Field {
    Field {
        key: key.to_string(),
        value: FieldValue::Str(value.into()),
    }
}

/// `zap.Error`: key `error`.
pub fn err(e: impl std::fmt::Display) -> Field {
    s("error", e.to_string())
}

/// `zap.Int`.
pub fn int(key: &str, value: i64) -> Field {
    Field {
        key: key.to_string(),
        value: FieldValue::Int(value),
    }
}

/// `zap.Bool`.
pub fn boolean(key: &str, value: bool) -> Field {
    Field {
        key: key.to_string(),
        value: FieldValue::Bool(value),
    }
}

/// `zap.Any` of a JSON-like value.
pub fn any(key: &str, value: &Value) -> Field {
    let v = match value {
        Value::String(s) => FieldValue::Str(s.clone()),
        Value::Bool(b) => FieldValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                FieldValue::Int(i)
            } else if let Some(u) = n.as_u64() {
                FieldValue::Uint(u)
            } else {
                FieldValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        // zap reflects other values with `json.Encoder`, without HTML escaping.
        other => FieldValue::Json(
            String::from_utf8(crate::gojson::to_vec_no_html(other)).unwrap_or_default(),
        ),
    };
    Field {
        key: key.to_string(),
        value: v,
    }
}

enum Sink {
    Stdout,
    Stderr,
    File(Mutex<File>),
}

impl Sink {
    fn write(&self, line: &[u8]) {
        let _ = match self {
            Sink::Stdout => std::io::stdout().lock().write_all(line),
            Sink::Stderr => std::io::stderr().lock().write_all(line),
            Sink::File(f) => f.lock().write_all(line),
        };
    }

    fn open(path: &str) -> Result<Sink, String> {
        match path {
            "stdout" => return Ok(Sink::Stdout),
            "stderr" => return Ok(Sink::Stderr),
            _ => {}
        }
        let file_path = match path.strip_prefix("file://") {
            Some(p) => p.to_string(),
            None => path.to_string(),
        };
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .map(|f| Sink::File(Mutex::new(f)))
            .map_err(|e| crate::config::go_open_error(&file_path, &e))
    }
}

struct Sampler {
    initial: u64,
    thereafter: u64,
    counts: HashMap<(Level, String), (u64, Instant)>,
}

impl Sampler {
    /// zapcore sampler: per (level, message) and per second, log the first `initial` entries
    /// then every `thereafter`-th.
    fn allow(&mut self, level: Level, msg: &str) -> bool {
        if level > Level::Fatal {
            return true;
        }
        let now = Instant::now();
        let e = self
            .counts
            .entry((level, msg.to_string()))
            .or_insert((0, now));
        if now.duration_since(e.1) >= Duration::from_secs(1) {
            *e = (0, now);
        }
        e.0 += 1;
        let n = e.0;
        !(n > self.initial
            && (self.thereafter == 0 || !(n - self.initial).is_multiple_of(self.thereafter)))
    }
}

struct Inner {
    level: Level,
    json: bool,
    cfg: crate::config::zap::EncoderConfig,
    line_ending: String,
    separator: String,
    /// Initial fields, already encoded as `"k":v` members.
    initial: String,
    outputs: Vec<Sink>,
    caller: bool,
    stack_level: Option<Level>,
    sampler: Option<Mutex<Sampler>>,
}

/// A configured logger. Cheap to clone.
#[derive(Clone)]
pub struct Logger {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger").finish_non_exhaustive()
    }
}

/// zap's JSON string escaping (no HTML escaping, unlike `encoding/json`).
fn zap_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// zap's float encoding: `strconv.AppendFloat(f, 'f', -1, 64)`, special values as strings.
fn zap_float(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("\"NaN\"");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "\"+Inf\"" } else { "\"-Inf\"" });
    } else if f == 0.0 {
        out.push_str(if f.is_sign_negative() { "-0" } else { "0" });
    } else {
        let (neg, digits, exp) = crate::gojson::shortest_digits(f);
        if neg {
            out.push('-');
        }
        crate::gojson::push_plain(out, &digits, exp);
    }
}

/// Appends a `"key":value` member; `spaced` is the JSON encoder of zap's console encoder
/// (`"key": value`, `, ` between members).
fn push_field(out: &mut String, f: &Field, spaced: bool) {
    if !out.is_empty() && !out.ends_with('{') {
        out.push_str(if spaced { ", " } else { "," });
    }
    zap_string(out, &f.key);
    out.push_str(if spaced { ": " } else { ":" });
    match &f.value {
        FieldValue::Str(v) => zap_string(out, v),
        FieldValue::Int(v) => {
            let _ = write!(out, "{v}");
        }
        FieldValue::Uint(v) => {
            let _ = write!(out, "{v}");
        }
        FieldValue::Float(v) => zap_float(out, *v),
        FieldValue::Bool(v) => out.push_str(if *v { "true" } else { "false" }),
        FieldValue::Json(v) => out.push_str(v),
    }
}

/// A time as a zap time encoder appends it.
enum EncodedTime {
    Str(String),
    Float(f64),
    Int(i64),
}

impl EncodedTime {
    /// In JSON (the time key, or a time field).
    fn push_json(&self, out: &mut String) {
        match self {
            EncodedTime::Str(s) => zap_string(out, s),
            EncodedTime::Float(f) => zap_float(out, *f),
            EncodedTime::Int(i) => {
                let _ = write!(out, "{i}");
            }
        }
    }

    /// In the first column of a console line (`fmt.Fprint` of the appended value).
    fn console(&self) -> String {
        match self {
            EncodedTime::Str(s) => s.clone(),
            EncodedTime::Float(f) => {
                let mut s = String::new();
                crate::gojson::fmt_float_v(*f, &mut s);
                s
            }
            EncodedTime::Int(i) => i.to_string(),
        }
    }
}

/// `EncodeTime` of the configuration for `t`; nanoseconds when there is none (zap's
/// fallback).
fn encode_time(encoder: Option<&TimeEncoder>, t: &DateTime<FixedOffset>) -> EncodedTime {
    let nanos = t.timestamp_nanos_opt().unwrap_or(0);
    match encoder {
        Some(TimeEncoder::Iso8601) => EncodedTime::Str(gotime::format(t, gotime::ISO8601)),
        Some(TimeEncoder::Rfc3339) => EncodedTime::Str(gotime::format(t, gotime::RFC3339)),
        Some(TimeEncoder::Rfc3339Nano) => EncodedTime::Str(gotime::format(t, gotime::RFC3339_NANO)),
        Some(TimeEncoder::Layout(layout)) => EncodedTime::Str(gotime::format(t, layout)),
        Some(TimeEncoder::Millis) => EncodedTime::Float(nanos as f64 / 1e6),
        Some(TimeEncoder::Epoch) => EncodedTime::Float(nanos as f64 / 1e9),
        Some(TimeEncoder::Nanos) | None => EncodedTime::Int(nanos),
    }
}

/// `zap.Any` of an `initialFields` value decoded by yaml.v3.
fn initial_field(key: &str, value: &AnyValue, encoder: Option<&TimeEncoder>) -> Field {
    match value {
        AnyValue::Json(v) => any(key, v),
        AnyValue::Float(f) => Field {
            key: key.to_string(),
            value: FieldValue::Float(*f),
        },
        AnyValue::Time(t) => {
            let mut json = String::new();
            encode_time(encoder, t).push_json(&mut json);
            Field {
                key: key.to_string(),
                value: FieldValue::Json(json),
            }
        }
        // A reflected value that `json.Marshal` refuses: zap logs the error instead.
        AnyValue::Unsupported(e) => s(&format!("{key}Error"), e.as_str()),
    }
}

fn color_level(level: Level, text: &str) -> String {
    let code = match level {
        Level::Debug => 35,
        Level::Info => 34,
        Level::Warn => 33,
        _ => 31,
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

impl Logger {
    /// Builds a logger like `zap.Config.Build`, with zap's validation order and messages.
    pub fn build(cfg: &ZapConfig) -> Result<Logger, String> {
        let enc = &cfg.encoder;
        if !enc.time_key.is_empty() && enc.time_encoder.is_none() {
            return Err("missing EncodeTime in EncoderConfig".to_string());
        }
        let json = match cfg.encoding.as_str() {
            "" => return Err("no encoder name specified".to_string()),
            "json" => true,
            "console" => false,
            other => return Err(format!("no encoder registered for name {other:?}")),
        };
        let mut outputs = Vec::new();
        for p in cfg.output_paths.iter().flatten() {
            outputs.push(Sink::open(p)?);
        }
        for p in cfg.error_output_paths.iter().flatten() {
            Sink::open(p)?;
        }
        let Some(level) = cfg.level else {
            return Err("missing Level".to_string());
        };
        // zap sorts the initial fields by key.
        let mut initial = String::new();
        let mut fields: Vec<&(String, AnyValue)> = cfg.initial_fields.iter().flatten().collect();
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in fields {
            let f = initial_field(k, v, enc.time_encoder.as_ref());
            push_field(&mut initial, &f, !json);
        }
        let line_ending = if enc.skip_line_ending {
            String::new()
        } else if enc.line_ending.is_empty() {
            "\n".to_string()
        } else {
            enc.line_ending.clone()
        };
        let separator = if enc.console_separator.is_empty() {
            "\t".to_string()
        } else {
            enc.console_separator.clone()
        };
        let stack_level = if cfg.disable_stacktrace {
            None
        } else if cfg.development {
            Some(Level::Warn)
        } else {
            Some(Level::Error)
        };
        let sampler = cfg.sampling.as_ref().map(|s| {
            Mutex::new(Sampler {
                initial: s.initial.max(0) as u64,
                thereafter: s.thereafter.max(0) as u64,
                counts: HashMap::new(),
            })
        });
        Ok(Logger {
            inner: Arc::new(Inner {
                level,
                json,
                cfg: enc.clone(),
                line_ending,
                separator,
                initial,
                outputs,
                caller: !cfg.disable_caller,
                stack_level,
                sampler,
            }),
        })
    }

    /// A logger that writes nothing (tests, or before the configuration is loaded).
    pub fn nop() -> Logger {
        Logger {
            inner: Arc::new(Inner {
                level: Level::Fatal,
                json: true,
                cfg: Default::default(),
                line_ending: "\n".into(),
                separator: "\t".into(),
                initial: String::new(),
                outputs: Vec::new(),
                caller: false,
                stack_level: None,
                sampler: None,
            }),
        }
    }

    pub fn enabled(&self, level: Level) -> bool {
        level >= self.inner.level && !self.inner.outputs.is_empty()
    }

    fn caller_string(&self, loc: &Location<'_>) -> String {
        let file = loc.file().replace('\\', "/");
        let file = match self.inner.cfg.caller_encoder {
            Some(CallerEncoder::Full) => file,
            // zap's short form keeps the last directory and the file name.
            _ => {
                let parts: Vec<&str> = file.rsplitn(3, '/').collect();
                match parts.as_slice() {
                    [name, dir, _] => format!("{dir}/{name}"),
                    _ => file.clone(),
                }
            }
        };
        format!("{file}:{}", loc.line())
    }

    #[track_caller]
    pub fn log(&self, level: Level, msg: &str, fields: &[Field]) {
        if !self.enabled(level) {
            return;
        }
        if let Some(s) = &self.inner.sampler
            && !s.lock().allow(level, msg)
        {
            return;
        }
        let caller = if self.inner.caller {
            Some(self.caller_string(Location::caller()))
        } else {
            None
        };
        let stack = match self.inner.stack_level {
            Some(l) if level >= l && !self.inner.cfg.stacktrace_key.is_empty() => {
                Some(std::backtrace::Backtrace::force_capture().to_string())
            }
            _ => None,
        };
        let line = if self.inner.json {
            self.encode_json(level, msg, fields, caller.as_deref(), stack.as_deref())
        } else {
            self.encode_console(level, msg, fields, caller.as_deref(), stack.as_deref())
        };
        for o in &self.inner.outputs {
            o.write(line.as_bytes());
        }
    }

    fn level_text(&self, level: Level) -> Option<String> {
        self.inner.cfg.level_encoder.map(|e| match e {
            LevelEncoder::Lowercase => level.lower().to_string(),
            LevelEncoder::Capital => level.capital().to_string(),
            LevelEncoder::LowercaseColor => color_level(level, level.lower()),
            LevelEncoder::CapitalColor => color_level(level, level.capital()),
        })
    }

    fn encode_json(
        &self,
        level: Level,
        msg: &str,
        fields: &[Field],
        caller: Option<&str>,
        stack: Option<&str>,
    ) -> String {
        let c = &self.inner.cfg;
        let mut out = String::with_capacity(256);
        out.push('{');
        let sep = |out: &mut String| {
            if !out.ends_with('{') {
                out.push(',');
            }
        };
        if !c.level_key.is_empty()
            && let Some(t) = self.level_text(level)
        {
            sep(&mut out);
            zap_string(&mut out, &c.level_key);
            out.push(':');
            zap_string(&mut out, &t);
        }
        if !c.time_key.is_empty() {
            sep(&mut out);
            zap_string(&mut out, &c.time_key);
            out.push(':');
            encode_time(c.time_encoder.as_ref(), &gotime::now_local()).push_json(&mut out);
        }
        if let Some(caller) = caller {
            if !c.caller_key.is_empty() {
                sep(&mut out);
                zap_string(&mut out, &c.caller_key);
                out.push(':');
                zap_string(&mut out, caller);
            }
            if !c.function_key.is_empty() {
                sep(&mut out);
                zap_string(&mut out, &c.function_key);
                out.push(':');
                zap_string(&mut out, "");
            }
        }
        if !c.message_key.is_empty() {
            sep(&mut out);
            zap_string(&mut out, &c.message_key);
            out.push(':');
            zap_string(&mut out, msg);
        }
        if !self.inner.initial.is_empty() {
            sep(&mut out);
            out.push_str(&self.inner.initial);
        }
        for f in fields {
            push_field(&mut out, f, false);
        }
        if let Some(st) = stack {
            sep(&mut out);
            zap_string(&mut out, &c.stacktrace_key);
            out.push(':');
            zap_string(&mut out, st);
        }
        out.push('}');
        out.push_str(&self.inner.line_ending);
        out
    }

    fn encode_console(
        &self,
        level: Level,
        msg: &str,
        fields: &[Field],
        caller: Option<&str>,
        stack: Option<&str>,
    ) -> String {
        let c = &self.inner.cfg;
        let mut parts: Vec<String> = Vec::new();
        if !c.time_key.is_empty() {
            parts.push(encode_time(c.time_encoder.as_ref(), &gotime::now_local()).console());
        }
        if !c.level_key.is_empty()
            && let Some(t) = self.level_text(level)
        {
            parts.push(t);
        }
        if let Some(caller) = caller
            && !c.caller_key.is_empty()
        {
            parts.push(caller.to_string());
        }
        if !c.message_key.is_empty() {
            parts.push(msg.to_string());
        }
        let mut ctx = String::new();
        if !self.inner.initial.is_empty() {
            ctx.push_str(&self.inner.initial);
        }
        for f in fields {
            push_field(&mut ctx, f, true);
        }
        if !ctx.is_empty() {
            parts.push(format!("{{{ctx}}}"));
        }
        let mut line = parts.join(&self.inner.separator);
        if let Some(st) = stack
            && !c.stacktrace_key.is_empty()
        {
            line.push('\n');
            line.push_str(st);
        }
        line.push_str(&self.inner.line_ending);
        line
    }

    #[track_caller]
    pub fn debug(&self, msg: &str, fields: &[Field]) {
        self.log(Level::Debug, msg, fields);
    }

    #[track_caller]
    pub fn info(&self, msg: &str, fields: &[Field]) {
        self.log(Level::Info, msg, fields);
    }

    #[track_caller]
    pub fn warn(&self, msg: &str, fields: &[Field]) {
        self.log(Level::Warn, msg, fields);
    }

    #[track_caller]
    pub fn error(&self, msg: &str, fields: &[Field]) {
        self.log(Level::Error, msg, fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> ZapConfig {
        crate::config::parse(yaml).unwrap().logger
    }

    #[test]
    fn build_errors_like_zap() {
        assert_eq!(
            Logger::build(&ZapConfig::default()).unwrap_err(),
            "no encoder name specified"
        );
        let c = cfg("logger:\n  encoding: json\n  encoderConfig:\n    timeKey: ts\n");
        assert_eq!(
            Logger::build(&c).unwrap_err(),
            "missing EncodeTime in EncoderConfig"
        );
        let c = cfg("logger:\n  encoding: xml\n");
        assert_eq!(
            Logger::build(&c).unwrap_err(),
            "no encoder registered for name \"xml\""
        );
        let c = cfg("logger:\n  encoding: json\n");
        assert_eq!(Logger::build(&c).unwrap_err(), "missing Level");
    }

    #[test]
    fn json_line_layout() {
        let c = cfg(
            "logger:\n  level: info\n  encoding: json\n  encoderConfig:\n    messageKey: message\n    levelKey: level\n    levelEncoder: ''\n  initialFields:\n    b: 2\n    a: x<y\n",
        );
        let l = Logger::build(&c).unwrap();
        let line = l.encode_json(
            Level::Info,
            "Hello \"x\"",
            &[s("topic", "server"), err("boom")],
            None,
            None,
        );
        assert_eq!(
            line,
            "{\"level\":\"info\",\"message\":\"Hello \\\"x\\\"\",\"a\":\"x<y\",\"b\":2,\"topic\":\"server\",\"error\":\"boom\"}\n"
        );
        // The console encoder writes the fields with zap's spaced JSON encoder.
        let c = cfg(
            "logger:\n  level: info\n  encoding: console\n  encoderConfig:\n    messageKey: message\n    levelKey: level\n    levelEncoder: ''\n  initialFields:\n    b: 2\n    a: x<y\n",
        );
        let l = Logger::build(&c).unwrap();
        let line = l.encode_console(Level::Warn, "m", &[s("k", "v")], None, None);
        assert_eq!(line, "warn\tm\t{\"a\": \"x<y\", \"b\": 2, \"k\": \"v\"}\n");
    }

    /// `initialFields` as zap receives them from yaml.v3: timestamps are `time.Time` (written
    /// with the time encoder), `.inf` and `.nan` are floats, nested values are reflected
    /// without HTML escaping, and a nested value JSON cannot hold gives a `<key>Error` field.
    #[test]
    fn initial_fields_like_zap() {
        let c = cfg(concat!(
            "logger:\n  level: info\n  encoding: json\n",
            "  encoderConfig: {messageKey: m, timeEncoder: iso8601}\n",
            "  initialFields:\n",
            "    date: 2026-10-18\n",
            "    stamp: 2026-10-18T12:30:00.5+02:00\n",
            "    notdate: 2026-13-45\n",
            "    inf: .inf\n",
            "    nan: .nan\n",
            "    nested: {b: '<x>', a: [1, 2026-10-18 01:02:03]}\n",
            "    bad: {a: 1, z: .inf, b: -.inf}\n",
        ));
        let l = Logger::build(&c).unwrap();
        assert_eq!(
            l.encode_json(Level::Info, "x", &[], None, None),
            concat!(
                "{\"m\":\"x\",\"badError\":\"json: unsupported value: -Inf\",",
                "\"date\":\"2026-10-18T00:00:00.000Z\",\"inf\":\"+Inf\",\"nan\":\"NaN\",",
                "\"nested\":{\"a\":[1,\"2026-10-18T01:02:03Z\"],\"b\":\"<x>\"},",
                "\"notdate\":\"2026-13-45\",\"stamp\":\"2026-10-18T12:30:00.500+0200\"}\n"
            )
        );
        let c = cfg(
            "logger:\n  level: info\n  encoding: json\n  encoderConfig: {timeEncoder: epoch}\n  initialFields: {d: 1970-01-02}\n",
        );
        let l = Logger::build(&c).unwrap();
        assert_eq!(
            l.encode_json(Level::Info, "x", &[], None, None),
            "{\"d\":86400}\n"
        );
    }

    /// The first column of a console line is `fmt.Fprint` of what the time encoder appended.
    #[test]
    fn console_time_column() {
        assert_eq!(
            EncodedTime::Float(1792351239514.7505).console(),
            "1.7923512395147505e+12"
        );
        assert_eq!(EncodedTime::Float(1.5).console(), "1.5");
        assert_eq!(
            EncodedTime::Int(1792351215722291500).console(),
            "1792351215722291500"
        );
        let mut json = String::new();
        EncodedTime::Float(1792351239514.7505).push_json(&mut json);
        assert_eq!(json, "1792351239514.7505");
    }
}
