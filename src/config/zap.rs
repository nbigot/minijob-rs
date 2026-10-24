//! `zap.Config` as decoded from YAML by the Go server.

use super::yaml::{Decoder, Kind, Node};

/// zap levels, in increasing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
    DPanic,
    Panic,
    Fatal,
}

impl Level {
    /// zapcore `Level.UnmarshalText`: lowercase or uppercase names, `""` = info.
    pub fn parse(text: &str) -> Result<Level, String> {
        let try_parse = |t: &str| match t {
            "debug" | "DEBUG" => Some(Level::Debug),
            "info" | "INFO" | "" => Some(Level::Info),
            "warn" | "WARN" => Some(Level::Warn),
            "error" | "ERROR" => Some(Level::Error),
            "dpanic" | "DPANIC" => Some(Level::DPanic),
            "panic" | "PANIC" => Some(Level::Panic),
            "fatal" | "FATAL" => Some(Level::Fatal),
            _ => None,
        };
        try_parse(text)
            .or_else(|| try_parse(&text.to_lowercase()))
            .ok_or_else(|| format!("unrecognized level: {text:?}"))
    }

    pub fn lower(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::DPanic => "dpanic",
            Level::Panic => "panic",
            Level::Fatal => "fatal",
        }
    }

    pub fn capital(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
            Level::DPanic => "DPANIC",
            Level::Panic => "PANIC",
            Level::Fatal => "FATAL",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelEncoder {
    Lowercase,
    LowercaseColor,
    Capital,
    CapitalColor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimeEncoder {
    Rfc3339Nano,
    Rfc3339,
    Iso8601,
    Millis,
    Nanos,
    Epoch,
    /// Go time layout (`timeEncoder: {layout: ...}`).
    Layout(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurationEncoder {
    Seconds,
    Nanos,
    Millis,
    String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallerEncoder {
    Short,
    Full,
}

/// `zapcore.EncoderConfig`. Encoders absent from the YAML are `None` (nil in Go).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncoderConfig {
    pub message_key: String,
    pub level_key: String,
    pub time_key: String,
    pub name_key: String,
    pub caller_key: String,
    pub function_key: String,
    pub stacktrace_key: String,
    pub skip_line_ending: bool,
    pub line_ending: String,
    pub level_encoder: Option<LevelEncoder>,
    pub time_encoder: Option<TimeEncoder>,
    pub duration_encoder: Option<DurationEncoder>,
    pub caller_encoder: Option<CallerEncoder>,
    pub console_separator: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sampling {
    pub initial: i64,
    pub thereafter: i64,
}

/// `zap.Config`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZapConfig {
    /// `None` when the key is absent: zap refuses to build (`missing Level`).
    pub level: Option<Level>,
    pub development: bool,
    pub disable_caller: bool,
    pub disable_stacktrace: bool,
    pub sampling: Option<Sampling>,
    pub encoding: String,
    pub encoder: EncoderConfig,
    pub output_paths: Option<Vec<String>>,
    pub error_output_paths: Option<Vec<String>>,
    pub initial_fields: Option<Vec<(String, super::yaml::AnyValue)>>,
}

/// Text of a scalar node for `UnmarshalText`; `None` for null or non-scalars.
fn text_of(dec: &mut Decoder, node: &Node, go_type: &str) -> Option<String> {
    match &node.kind {
        Kind::Scalar { .. } => dec.text(node),
        _ => {
            dec.type_error(node, go_type);
            None
        }
    }
}

fn time_encoder_from_text(text: &str) -> TimeEncoder {
    match text {
        "rfc3339nano" | "RFC3339Nano" => TimeEncoder::Rfc3339Nano,
        "rfc3339" | "RFC3339" => TimeEncoder::Rfc3339,
        "iso8601" | "ISO8601" => TimeEncoder::Iso8601,
        "millis" => TimeEncoder::Millis,
        "nanos" => TimeEncoder::Nanos,
        _ => TimeEncoder::Epoch,
    }
}

fn decode_encoder(dec: &mut Decoder, node: &Node, out: &mut EncoderConfig) {
    dec.mapping(node, "zapcore.EncoderConfig", |d, k, v| {
        match k {
            "messageKey" => d.string(v, &mut out.message_key),
            "levelKey" => d.string(v, &mut out.level_key),
            "timeKey" => d.string(v, &mut out.time_key),
            "nameKey" => d.string(v, &mut out.name_key),
            "callerKey" => d.string(v, &mut out.caller_key),
            "functionKey" => d.string(v, &mut out.function_key),
            "stacktraceKey" => d.string(v, &mut out.stacktrace_key),
            "skipLineEnding" => d.bool(v, &mut out.skip_line_ending),
            "lineEnding" => d.string(v, &mut out.line_ending),
            "consoleSeparator" => d.string(v, &mut out.console_separator),
            "levelEncoder" => {
                if let Some(t) = text_of(d, v, "zapcore.LevelEncoder") {
                    out.level_encoder = Some(match t.as_str() {
                        "capital" => LevelEncoder::Capital,
                        "capitalColor" => LevelEncoder::CapitalColor,
                        "color" => LevelEncoder::LowercaseColor,
                        _ => LevelEncoder::Lowercase,
                    });
                }
            }
            "timeEncoder" => match &v.kind {
                // TimeEncoder.UnmarshalYAML: {layout: "..."} or a name.
                Kind::Map(_) => {
                    let mut layout = String::new();
                    d.mapping(v, "struct { Layout string }", |d, k, v| {
                        if k == "layout" {
                            d.string(v, &mut layout);
                            true
                        } else {
                            false
                        }
                    });
                    out.time_encoder = Some(TimeEncoder::Layout(layout));
                }
                // UnmarshalYAML tries a struct, then reports the error of a string.
                Kind::Seq(_) => d.type_error(v, "string"),
                _ => {
                    if let Some(t) = text_of(d, v, "zapcore.TimeEncoder") {
                        out.time_encoder = Some(time_encoder_from_text(&t));
                    }
                }
            },
            "durationEncoder" => {
                if let Some(t) = text_of(d, v, "zapcore.DurationEncoder") {
                    out.duration_encoder = Some(match t.as_str() {
                        "string" => DurationEncoder::String,
                        "nanos" => DurationEncoder::Nanos,
                        "ms" => DurationEncoder::Millis,
                        _ => DurationEncoder::Seconds,
                    });
                }
            }
            "callerEncoder" => {
                if let Some(t) = text_of(d, v, "zapcore.CallerEncoder") {
                    out.caller_encoder = Some(if t == "full" {
                        CallerEncoder::Full
                    } else {
                        CallerEncoder::Short
                    });
                }
            }
            // nameEncoder: only the full encoder exists.
            "nameEncoder" => {
                let _ = text_of(d, v, "zapcore.NameEncoder");
            }
            _ => return false,
        }
        true
    });
}

/// Decodes a `zap.Config` node.
pub fn decode(dec: &mut Decoder, node: &Node, out: &mut ZapConfig) {
    dec.mapping(node, "zap.Config", |d, k, v| {
        match k {
            // A mapping is decoded into the struct, which has no exported field: ignored.
            "level" if matches!(v.kind, Kind::Map(_)) => {}
            "level" => {
                if let Some(t) = text_of(d, v, "zap.AtomicLevel") {
                    match Level::parse(&t) {
                        Ok(l) => out.level = Some(l),
                        Err(e) => d.fail(e),
                    }
                }
            }
            "development" => d.bool(v, &mut out.development),
            "disableCaller" => d.bool(v, &mut out.disable_caller),
            "disableStacktrace" => d.bool(v, &mut out.disable_stacktrace),
            "sampling" => match &v.kind {
                Kind::Map(_) => {
                    let mut s = Sampling::default();
                    d.mapping(v, "zap.SamplingConfig", |d, k, v| {
                        match k {
                            "initial" => d.int(v, &mut s.initial, "int"),
                            "thereafter" => d.int(v, &mut s.thereafter, "int"),
                            _ => return false,
                        }
                        true
                    });
                    out.sampling = Some(s);
                }
                _ => {
                    if text_of(d, v, "zap.SamplingConfig").is_some() {
                        d.type_error(v, "zap.SamplingConfig");
                    } else {
                        out.sampling = None;
                    }
                }
            },
            "encoding" => d.string(v, &mut out.encoding),
            "encoderConfig" => decode_encoder(d, v, &mut out.encoder),
            "outputPaths" => d.string_list(v, &mut out.output_paths),
            "errorOutputPaths" => d.string_list(v, &mut out.error_output_paths),
            "initialFields" => d.any_map(v, &mut out.initial_fields),
            _ => return false,
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::yaml::AnyValue;

    #[test]
    fn levels() {
        assert_eq!(Level::parse("info"), Ok(Level::Info));
        assert_eq!(Level::parse(""), Ok(Level::Info));
        assert_eq!(Level::parse("WARN"), Ok(Level::Warn));
        assert_eq!(Level::parse("Debug"), Ok(Level::Debug));
        assert_eq!(
            Level::parse("verbose"),
            Err("unrecognized level: \"verbose\"".into())
        );
    }

    #[test]
    fn template_logger() {
        let c = crate::config::parse(
            "logger:\n  level: info\n  encoding: json\n  encoderConfig:\n    messageKey: message\n    levelEncoder: ''\n    timeEncoder: iso8601\n  outputPaths: [stdout]\n  initialFields:\n    app: minijob\n    n: 3\n",
        )
        .unwrap();
        let z = c.logger;
        assert_eq!(z.level, Some(Level::Info));
        assert_eq!(z.encoder.level_encoder, Some(LevelEncoder::Lowercase));
        assert_eq!(z.encoder.time_encoder, Some(TimeEncoder::Iso8601));
        assert_eq!(z.encoder.caller_encoder, None);
        assert_eq!(z.output_paths, Some(vec!["stdout".to_string()]));
        assert_eq!(
            z.initial_fields,
            Some(vec![
                ("app".into(), AnyValue::Json("minijob".into())),
                ("n".into(), AnyValue::Json(3.into()))
            ])
        );
        let c = crate::config::parse(
            "logger:\n  timeEncoder: x\n  encoderConfig:\n    timeEncoder: {layout: '15:04'}\n",
        )
        .unwrap();
        assert_eq!(
            c.logger.encoder.time_encoder,
            Some(TimeEncoder::Layout("15:04".into()))
        );
        let e = crate::config::parse("logger:\n  level: verbose\n").unwrap_err();
        assert_eq!(e, "unrecognized level: \"verbose\"");
    }

    #[test]
    fn level_names() {
        let all = [
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
            Level::DPanic,
            Level::Panic,
            Level::Fatal,
        ];
        for l in all {
            assert_eq!(Level::parse(l.lower()), Ok(l));
            assert_eq!(Level::parse(l.capital()), Ok(l));
            assert_eq!(l.capital(), l.lower().to_uppercase());
        }
    }

    /// The encoders and sampling chosen, as a Go program printed them for v1.1.2
    /// (encoder functions compared with zapcore's; "other" is a time layout).
    fn chosen(c: &ZapConfig) -> String {
        let e = &c.encoder;
        let level = e.level_encoder.map(|l| match l {
            LevelEncoder::Lowercase => "lower",
            LevelEncoder::LowercaseColor => "lowerColor",
            LevelEncoder::Capital => "capital",
            LevelEncoder::CapitalColor => "capitalColor",
        });
        let time = e.time_encoder.as_ref().map(|t| match t {
            TimeEncoder::Rfc3339Nano => "rfc3339nano",
            TimeEncoder::Rfc3339 => "rfc3339",
            TimeEncoder::Iso8601 => "iso8601",
            TimeEncoder::Millis => "millis",
            TimeEncoder::Nanos => "nanos",
            TimeEncoder::Epoch => "epoch",
            TimeEncoder::Layout(_) => "other",
        });
        let duration = e.duration_encoder.map(|d| match d {
            DurationEncoder::Seconds => "seconds",
            DurationEncoder::Nanos => "nanos",
            DurationEncoder::Millis => "ms",
            DurationEncoder::String => "string",
        });
        let caller = e.caller_encoder.map(|c| match c {
            CallerEncoder::Short => "short",
            CallerEncoder::Full => "full",
        });
        format!(
            "level={level:?} time={time:?} duration={duration:?} caller={caller:?} sampling={:?}",
            c.sampling.as_ref().map(|s| (s.initial, s.thereafter))
        )
    }

    #[test]
    fn encoders_like_go() {
        let cases = [
            (
                "logger:\n  encoderConfig:\n    levelEncoder: color\n    timeEncoder: {layout: '15:04', other: 1}\n    durationEncoder: ms\n    callerEncoder: short\n    nameEncoder: full\n",
                Ok(
                    "level=Some(\"lowerColor\") time=Some(\"other\") duration=Some(\"ms\") caller=Some(\"short\") sampling=None",
                ),
            ),
            (
                "logger:\n  encoderConfig:\n    levelEncoder: capitalColor\n    timeEncoder: millis\n    durationEncoder: nanos\n    callerEncoder: full\n    nameEncoder: x\n",
                Ok(
                    "level=Some(\"capitalColor\") time=Some(\"millis\") duration=Some(\"nanos\") caller=Some(\"full\") sampling=None",
                ),
            ),
            (
                "logger:\n  encoderConfig:\n    levelEncoder: [a]\n",
                Err(
                    "yaml: unmarshal errors:\n  line 3: cannot unmarshal !!seq into zapcore.LevelEncoder",
                ),
            ),
            (
                "logger:\n  encoderConfig:\n    nameEncoder: {a: 1}\n",
                Err(
                    "yaml: unmarshal errors:\n  line 3: cannot unmarshal !!map into zapcore.NameEncoder",
                ),
            ),
            (
                "logger:\n  encoderConfig:\n    durationEncoder: [1]\n    callerEncoder: {a: b}\n",
                Err(
                    "yaml: unmarshal errors:\n  line 3: cannot unmarshal !!seq into zapcore.DurationEncoder\n  line 4: cannot unmarshal !!map into zapcore.CallerEncoder",
                ),
            ),
            (
                "logger:\n  encoderConfig:\n    timeEncoder: [x]\n",
                Err("yaml: unmarshal errors:\n  line 3: cannot unmarshal !!seq into string"),
            ),
            (
                "logger:\n  encoderConfig:\n    timeEncoder: nanos\n    durationEncoder: string\n    levelEncoder: capital\n",
                Ok(
                    "level=Some(\"capital\") time=Some(\"nanos\") duration=Some(\"string\") caller=None sampling=None",
                ),
            ),
            (
                "logger:\n  encoderConfig:\n    timeEncoder: RFC3339\n    durationEncoder: other\n",
                Ok(
                    "level=None time=Some(\"rfc3339\") duration=Some(\"seconds\") caller=None sampling=None",
                ),
            ),
            (
                "logger:\n  encoderConfig:\n    timeEncoder: rfc3339nano\n",
                Ok("level=None time=Some(\"rfc3339nano\") duration=None caller=None sampling=None"),
            ),
            (
                "logger:\n  encoderConfig:\n    levelEncoder: ~\n    timeEncoder: ~\n    durationEncoder: ~\n    callerEncoder: ~\n    nameEncoder: ~\n",
                Ok("level=None time=None duration=None caller=None sampling=None"),
            ),
            (
                "logger:\n  sampling: {initial: 1, thereafter: 2, other: 3}\n",
                Ok("level=None time=None duration=None caller=None sampling=Some((1, 2))"),
            ),
            (
                "logger:\n  sampling: 5\n",
                Err(
                    "yaml: unmarshal errors:\n  line 2: cannot unmarshal !!int `5` into zap.SamplingConfig",
                ),
            ),
            (
                "logger:\n  sampling: ~\n",
                Ok("level=None time=None duration=None caller=None sampling=None"),
            ),
            (
                "logger:\n  sampling: {initial: 1}\n  level: [info]\n",
                Err(
                    "yaml: unmarshal errors:\n  line 3: cannot unmarshal !!seq into zap.AtomicLevel",
                ),
            ),
            (
                "logger:\n  level: {a: 1}\n  sampling: [1]\n",
                Err(
                    "yaml: unmarshal errors:\n  line 3: cannot unmarshal !!seq into zap.SamplingConfig",
                ),
            ),
            (
                "logger:\n  level: ~\n",
                Ok("level=None time=None duration=None caller=None sampling=None"),
            ),
            (
                "logger:\n  level: DPANIC\n  encoderConfig: [1]\n",
                Err(
                    "yaml: unmarshal errors:\n  line 3: cannot unmarshal !!seq into zapcore.EncoderConfig",
                ),
            ),
            (
                "logger: [1]\n",
                Err("yaml: unmarshal errors:\n  line 1: cannot unmarshal !!seq into zap.Config"),
            ),
        ];
        for (yaml, want) in cases {
            let got = crate::config::parse(yaml).map(|c| chosen(&c.logger));
            let want = want.map(String::from).map_err(String::from);
            assert_eq!(got, want, "{yaml}");
        }
        let c = crate::config::parse("logger:\n  level: DPANIC\n").unwrap();
        assert_eq!(c.logger.level, Some(Level::DPanic));
        let c = crate::config::parse("logger:\n  level: ~\n").unwrap();
        assert_eq!(c.logger.level, None);
    }
}
