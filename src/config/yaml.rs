//! YAML decoding with the semantics of `gopkg.in/yaml.v3` decoding into Go structs.
//!
//! The document is parsed into a node tree (raw scalar text, style, tag and line kept), then
//! decoded field by field with yaml.v3's rules:
//!
//! - plain scalars are resolved like yaml.v3 (`0x1F`, `0o17`, `017` octal, `1_000`, `.inf`,
//!   `~`/`null`/empty = null); quoted scalars are strings;
//! - a string target accepts any scalar (raw text); bool targets also accept the YAML 1.1
//!   words `y/yes/on/n/no/off`; floats with an integral range fit integer targets (truncated);
//! - `null` leaves scalars and structs unchanged and sets slices and maps to nil;
//! - unknown keys are ignored, type errors are collected and reported together:
//!   `yaml: unmarshal errors:\n  line N: cannot unmarshal !!str `abc` into int`;
//! - anchors, aliases and merge keys (`<<`) are supported.

use std::collections::HashMap;
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser, Tag};
use yaml_rust2::scanner::{Marker, TScalarStyle};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Scalar { value: String, quoted: bool },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub kind: Kind,
    /// Short tag (`!!str`) when given explicitly.
    pub tag: Option<String>,
    /// 1-based line.
    pub line: usize,
}

fn short_tag(tag: &Tag) -> String {
    if tag.handle == "!!" {
        format!("!!{}", tag.suffix)
    } else if let Some(rest) = tag.suffix.strip_prefix("tag:yaml.org,2002:") {
        format!("!!{rest}")
    } else {
        format!("{}{}", tag.handle, tag.suffix)
    }
}

enum Building {
    Seq(Node, usize),
    Map(Node, Option<Node>, usize),
}

#[derive(Default)]
struct Builder {
    stack: Vec<Building>,
    anchors: HashMap<usize, Node>,
    doc: Option<Node>,
    docs_seen: usize,
    error: Option<String>,
}

impl Builder {
    fn push_value(&mut self, node: Node, anchor: usize) {
        if anchor > 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.stack.last_mut() {
            None => {
                if self.docs_seen == 1 && self.doc.is_none() {
                    self.doc = Some(node);
                }
            }
            Some(Building::Seq(parent, _)) => {
                if let Kind::Seq(items) = &mut parent.kind {
                    items.push(node);
                }
            }
            Some(Building::Map(parent, key, _)) => match key.take() {
                None => *key = Some(node),
                Some(k) => {
                    if let Kind::Map(entries) = &mut parent.kind {
                        entries.push((k, node));
                    }
                }
            },
        }
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, ev: Event, mark: Marker) {
        let line = mark.line().max(1);
        match ev {
            Event::DocumentStart => self.docs_seen += 1,
            Event::Scalar(value, style, anchor, tag) => {
                let quoted = !matches!(style, TScalarStyle::Plain);
                let node = Node {
                    kind: Kind::Scalar { value, quoted },
                    tag: tag.as_ref().map(short_tag),
                    line,
                };
                self.push_value(node, anchor);
            }
            Event::SequenceStart(anchor, tag) => self.stack.push(Building::Seq(
                Node {
                    kind: Kind::Seq(Vec::new()),
                    tag: tag.as_ref().map(short_tag),
                    line,
                },
                anchor,
            )),
            Event::MappingStart(anchor, tag) => self.stack.push(Building::Map(
                Node {
                    kind: Kind::Map(Vec::new()),
                    tag: tag.as_ref().map(short_tag),
                    line,
                },
                None,
                anchor,
            )),
            Event::SequenceEnd | Event::MappingEnd => match self.stack.pop() {
                Some(Building::Seq(node, anchor)) | Some(Building::Map(node, _, anchor)) => {
                    self.push_value(node, anchor)
                }
                None => self.error = Some("unbalanced YAML events".into()),
            },
            Event::Alias(id) => match self.anchors.get(&id).cloned() {
                Some(mut node) => {
                    node.line = line;
                    self.push_value(node, 0);
                }
                None => self.error = Some(format!("yaml: line {line}: unknown anchor")),
            },
            _ => {}
        }
    }
}

/// Parses the first document. `Ok(None)` when the stream has no document (Go returns `EOF`).
pub fn parse_first_document(text: &str) -> Result<Option<Node>, String> {
    let mut builder = Builder::default();
    let mut parser = Parser::new_from_str(text);
    // yaml.v3 only reads the first document; later documents are not even parsed by
    // Decoder.Decode, so stop at the first one.
    parser
        .load(&mut builder, false)
        .map_err(|e| format!("yaml: {e}"))?;
    if let Some(e) = builder.error {
        return Err(e);
    }
    Ok(builder.doc)
}

/// Resolved value of a plain scalar (yaml.v3 `resolve`).
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(String),
    Timestamp(String),
    Merge,
}

impl Resolved {
    fn tag(&self) -> &'static str {
        match self {
            Resolved::Null => "!!null",
            Resolved::Bool(_) => "!!bool",
            Resolved::Int(_) | Resolved::Uint(_) => "!!int",
            Resolved::Float(_) => "!!float",
            Resolved::Str(_) => "!!str",
            Resolved::Timestamp(_) => "!!timestamp",
            Resolved::Merge => "!!merge",
        }
    }
}

/// Go `strconv.ParseInt(s, 0, 64)` / `ParseUint`: optional sign, `0x`, `0o`, `0b` or a leading
/// `0` for octal.
fn parse_go_int(s: &str) -> Option<Resolved> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if body.is_empty() {
        return None;
    }
    let (radix, digits) = if let Some(r) = body.strip_prefix("0x").or(body.strip_prefix("0X")) {
        (16, r)
    } else if let Some(r) = body.strip_prefix("0o").or(body.strip_prefix("0O")) {
        (8, r)
    } else if let Some(r) = body.strip_prefix("0b").or(body.strip_prefix("0B")) {
        (2, r)
    } else if body.len() > 1 && body.starts_with('0') {
        (8, &body[1..])
    } else {
        (10, body)
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let mag = u64::from_str_radix(digits, radix).ok()?;
    if neg {
        if mag <= i64::MAX as u64 + 1 {
            Some(Resolved::Int((mag as i64).wrapping_neg()))
        } else {
            None
        }
    } else if mag <= i64::MAX as u64 {
        Some(Resolved::Int(mag as i64))
    } else {
        Some(Resolved::Uint(mag))
    }
}

fn is_yaml_float(s: &str) -> bool {
    // yamlStyleFloat = ^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if int_digits == 0 && i == frac_start {
            return false;
        }
    } else if int_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

fn looks_like_timestamp(s: &str) -> bool {
    // yaml.v3 accepts "2006-1-2", "2006-1-2T15:4:5.999999999Z07:00", "2006-1-2 15:4:5.999999999"
    // and "2006-1-2 15:4:5.999999999 -7". The date part is enough to classify it.
    let mut parts = s.splitn(2, ['T', 't', ' ']);
    let date = parts.next().unwrap_or("");
    let fields: Vec<&str> = date.split('-').collect();
    fields.len() == 3
        && fields[0].len() == 4
        && fields
            .iter()
            .all(|f| !f.is_empty() && f.len() <= 4 && f.bytes().all(|c| c.is_ascii_digit()))
}

/// yaml.v3 `resolve` for a scalar with no explicit tag.
pub fn resolve_plain(value: &str) -> Resolved {
    match value {
        "" | "~" | "null" | "Null" | "NULL" => return Resolved::Null,
        "true" | "True" | "TRUE" => return Resolved::Bool(true),
        "false" | "False" | "FALSE" => return Resolved::Bool(false),
        ".nan" | ".NaN" | ".NAN" => return Resolved::Float(f64::NAN),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return Resolved::Float(f64::INFINITY);
        }
        "-.inf" | "-.Inf" | "-.INF" => return Resolved::Float(f64::NEG_INFINITY),
        "<<" => return Resolved::Merge,
        _ => {}
    }
    let first = value.as_bytes()[0];
    match first {
        b'.' => {
            if let Ok(f) = value.parse::<f64>() {
                return Resolved::Float(f);
            }
        }
        b'0'..=b'9' | b'+' | b'-' => {
            if looks_like_timestamp(value) {
                return Resolved::Timestamp(value.to_string());
            }
            let plain = value.replace('_', "");
            if let Some(r) = parse_go_int(&plain) {
                return r;
            }
            if is_yaml_float(&plain)
                && let Ok(f) = plain.parse::<f64>()
            {
                return Resolved::Float(f);
            }
        }
        _ => {}
    }
    Resolved::Str(value.to_string())
}

/// Resolved value and tag of a scalar node.
pub fn resolve_node(node: &Node) -> (Resolved, String) {
    let Kind::Scalar { value, quoted } = &node.kind else {
        return (Resolved::Null, String::new());
    };
    let resolved = match node.tag.as_deref() {
        _ if *quoted => Resolved::Str(value.clone()),
        Some("!!str") | Some("!!binary") => Resolved::Str(value.clone()),
        Some("!!int") => match parse_go_int(&value.replace('_', "")) {
            Some(r) => r,
            None => Resolved::Str(value.clone()),
        },
        Some("!!float") => match value.parse::<f64>() {
            Ok(f) => Resolved::Float(f),
            Err(_) => resolve_plain(value),
        },
        _ => resolve_plain(value),
    };
    let tag = node
        .tag
        .clone()
        .unwrap_or_else(|| resolved.tag().to_string());
    (resolved, tag)
}

/// Go `strconv.Quote` (the `%#v` of a string): printable characters as is, the usual
/// backslash escapes, `\xNN` for other ASCII controls, `\uNNNN` / `\UNNNNNNNN` for other
/// non-printable characters (approximated with Rust's control and whitespace classes).
pub fn go_quote(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if c.is_control() || (c.is_whitespace() && c != ' ') || c == '\u{feff}' => {
                if (c as u32) < 0x10000 {
                    let _ = write!(out, "\\u{:04x}", c as u32);
                } else {
                    let _ = write!(out, "\\U{:08x}", c as u32);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Collects yaml.v3 type errors while decoding one document.
#[derive(Debug, Default)]
pub struct Decoder {
    errors: Vec<String>,
    /// A fatal error (yaml.v3 `fail`): stops and is returned as is.
    fatal: Option<String>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// yaml.v3 `terror`: `line N: cannot unmarshal <tag>[ `value`] into <type>`.
    pub fn type_error(&mut self, node: &Node, go_type: &str) {
        let (tag, value) = match &node.kind {
            Kind::Scalar { value, .. } => {
                let tag = resolve_node(node).1;
                let shown = if value.len() > 10 {
                    let mut end = 7;
                    while !value.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!(" `{}...`", &value[..end])
                } else {
                    format!(" `{value}`")
                };
                (tag, shown)
            }
            Kind::Seq(_) => (node.tag.clone().unwrap_or("!!seq".into()), String::new()),
            Kind::Map(_) => (node.tag.clone().unwrap_or("!!map".into()), String::new()),
        };
        self.errors.push(format!(
            "line {}: cannot unmarshal {tag}{value} into {go_type}",
            node.line
        ));
    }

    pub fn fail(&mut self, msg: String) {
        if self.fatal.is_none() {
            self.fatal = Some(msg);
        }
    }

    pub fn push_error(&mut self, msg: String) {
        self.errors.push(msg);
    }

    /// Final result: the first fatal error, or all type errors.
    pub fn finish(self) -> Result<(), String> {
        if let Some(f) = self.fatal {
            return Err(f);
        }
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "yaml: unmarshal errors:\n  {}",
                self.errors.join("\n  ")
            ))
        }
    }

    fn is_null(node: &Node) -> bool {
        matches!(node.kind, Kind::Scalar { .. }) && resolve_node(node).0 == Resolved::Null
    }

    /// Go `string`: any scalar, as written.
    pub fn string(&mut self, node: &Node, out: &mut String) {
        match &node.kind {
            Kind::Scalar { value, .. } => {
                if !Self::is_null(node) {
                    out.clone_from(value);
                }
            }
            _ => self.type_error(node, "string"),
        }
    }

    /// Go `bool`.
    pub fn bool(&mut self, node: &Node, out: &mut bool) {
        if !matches!(node.kind, Kind::Scalar { .. }) {
            return self.type_error(node, "bool");
        }
        match resolve_node(node).0 {
            Resolved::Null => {}
            Resolved::Bool(b) => *out = b,
            Resolved::Str(s) => match s.as_str() {
                "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => *out = true,
                "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => *out = false,
                _ => self.type_error(node, "bool"),
            },
            _ => self.type_error(node, "bool"),
        }
    }

    /// Go signed integer types (`int`, `int8`, ...), range checked.
    pub fn int<T: TryFrom<i64>>(&mut self, node: &Node, out: &mut T, go_type: &str) {
        if !matches!(node.kind, Kind::Scalar { .. }) {
            return self.type_error(node, go_type);
        }
        let v = match resolve_node(node).0 {
            Resolved::Null => return,
            Resolved::Int(i) => Some(i),
            Resolved::Uint(u) => i64::try_from(u).ok(),
            Resolved::Float(f) if f <= i64::MAX as f64 && f >= i64::MIN as f64 && !f.is_nan() => {
                Some(f as i64)
            }
            _ => None,
        };
        match v.and_then(|i| T::try_from(i).ok()) {
            Some(v) => *out = v,
            None => self.type_error(node, go_type),
        }
    }

    /// Go unsigned integer types (`uint`, ...), range checked.
    pub fn uint<T: TryFrom<u64>>(&mut self, node: &Node, out: &mut T, go_type: &str) {
        if !matches!(node.kind, Kind::Scalar { .. }) {
            return self.type_error(node, go_type);
        }
        let v = match resolve_node(node).0 {
            Resolved::Null => return,
            Resolved::Int(i) => u64::try_from(i).ok(),
            Resolved::Uint(u) => Some(u),
            Resolved::Float(f) if f >= 0.0 && f <= u64::MAX as f64 => Some(f as u64),
            _ => None,
        };
        match v.and_then(|u| T::try_from(u).ok()) {
            Some(v) => *out = v,
            None => self.type_error(node, go_type),
        }
    }

    /// Go `float64`.
    pub fn float(&mut self, node: &Node, out: &mut f64) {
        if !matches!(node.kind, Kind::Scalar { .. }) {
            return self.type_error(node, "float64");
        }
        match resolve_node(node).0 {
            Resolved::Null => {}
            Resolved::Int(i) => *out = i as f64,
            Resolved::Uint(u) => *out = u as f64,
            Resolved::Float(f) => *out = f,
            _ => self.type_error(node, "float64"),
        }
    }

    /// Go `[]string`.
    pub fn string_list(&mut self, node: &Node, out: &mut Option<Vec<String>>) {
        match &node.kind {
            Kind::Seq(items) => {
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    let mut s = String::new();
                    self.string(item, &mut s);
                    list.push(s);
                }
                *out = Some(list);
            }
            Kind::Scalar { .. } if Self::is_null(node) => *out = None,
            _ => self.type_error(node, "[]string"),
        }
    }

    /// Go `map[string]interface{}` as JSON values (zap `initialFields`).
    pub fn any_map(&mut self, node: &Node, out: &mut Option<Vec<(String, serde_json::Value)>>) {
        match &node.kind {
            Kind::Map(_) => {
                let mut list: Vec<(String, serde_json::Value)> = Vec::new();
                self.mapping(node, "map[string]interface {}", |dec, key, value| {
                    let v = dec.any(value);
                    match list.iter_mut().find(|(k, _)| k == key) {
                        Some(slot) => slot.1 = v,
                        None => list.push((key.to_string(), v)),
                    }
                    true
                });
                *out = Some(list);
            }
            Kind::Scalar { .. } if Self::is_null(node) => *out = None,
            _ => self.type_error(node, "map[string]interface {}"),
        }
    }

    /// Go `interface{}` value converted to JSON.
    pub fn any(&mut self, node: &Node) -> serde_json::Value {
        use serde_json::Value;
        match &node.kind {
            Kind::Scalar { .. } => match resolve_node(node).0 {
                Resolved::Null | Resolved::Merge => Value::Null,
                Resolved::Bool(b) => Value::Bool(b),
                Resolved::Int(i) => Value::from(i),
                Resolved::Uint(u) => Value::from(u),
                Resolved::Float(f) => serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                Resolved::Str(s) | Resolved::Timestamp(s) => Value::String(s),
            },
            Kind::Seq(items) => Value::Array(items.iter().map(|i| self.any(i)).collect()),
            Kind::Map(_) => {
                let mut map = serde_json::Map::new();
                self.mapping(node, "map[string]interface {}", |dec, key, value| {
                    let v = dec.any(value);
                    map.insert(key.to_string(), v);
                    true
                });
                Value::Object(map)
            }
        }
    }

    /// yaml.v3 `uniqueKeys`: every key that repeats an earlier one of the same mapping is an
    /// error (`mapping key "k" already defined at line N`), whether or not the target has a
    /// field for it. Returns true when there is one; the mapping is then not decoded.
    fn duplicate_keys(&mut self, entries: &[(Node, Node)]) -> bool {
        // yaml.v3 compares the node kind and the raw value (empty for collections).
        fn ident(n: &Node) -> (u8, &str) {
            match &n.kind {
                Kind::Scalar { value, .. } => (0, value.as_str()),
                Kind::Seq(_) => (1, ""),
                Kind::Map(_) => (2, ""),
            }
        }
        let before = self.errors.len();
        for (i, (ki, _)) in entries.iter().enumerate() {
            for (kj, _) in &entries[i + 1..] {
                if ident(ki) == ident(kj) {
                    self.errors.push(format!(
                        "line {}: mapping key {} already defined at line {}",
                        kj.line,
                        go_quote(ident(kj).1),
                        ki.line
                    ));
                }
            }
        }
        self.errors.len() > before
    }

    /// Iterates the members of a mapping node (struct or map target), applying merge keys
    /// (`<<`) after the explicit keys, which take precedence. `field` returns `false` for an
    /// unknown key (unknown keys are ignored, as in yaml.v3).
    pub fn mapping<F>(&mut self, node: &Node, go_type: &str, mut field: F)
    where
        F: FnMut(&mut Decoder, &str, &Node) -> bool,
    {
        let mut seen: Vec<String> = Vec::new();
        self.mapping_inner(node, go_type, &mut field, &mut seen, true);
    }

    fn mapping_inner<F>(
        &mut self,
        node: &Node,
        go_type: &str,
        field: &mut F,
        seen: &mut Vec<String>,
        explicit: bool,
    ) where
        F: FnMut(&mut Decoder, &str, &Node) -> bool,
    {
        let Kind::Map(entries) = &node.kind else {
            if !Self::is_null(node) {
                self.type_error(node, go_type);
            }
            return;
        };
        if self.duplicate_keys(entries) {
            return;
        }
        let mut merges = Vec::new();
        for (k, v) in entries {
            let is_merge = matches!(&k.kind, Kind::Scalar { quoted: false, .. })
                && k.tag.is_none()
                && matches!(resolve_node(k).0, Resolved::Merge);
            if is_merge {
                merges.push(v);
                continue;
            }
            let key = match &k.kind {
                Kind::Scalar { value, .. } => value.clone(),
                _ => {
                    self.type_error(k, "string");
                    continue;
                }
            };
            if seen.contains(&key) && !explicit {
                // A key already set explicitly (or by an earlier merge) wins over merged ones.
                continue;
            }
            field(self, &key, v);
            seen.push(key);
        }
        for m in merges {
            match &m.kind {
                Kind::Map(_) => self.mapping_inner(m, go_type, field, seen, false),
                Kind::Seq(items) => {
                    for item in items {
                        if matches!(item.kind, Kind::Map(_)) {
                            self.mapping_inner(item, go_type, field, seen, false);
                        } else {
                            self.fail(
                                "yaml: map merge requires map or sequence of maps as the value"
                                    .to_string(),
                            );
                        }
                    }
                }
                _ => self.fail(
                    "yaml: map merge requires map or sequence of maps as the value".to_string(),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Node {
        parse_first_document(s).unwrap().unwrap()
    }

    fn get<'a>(n: &'a Node, key: &str) -> &'a Node {
        let Kind::Map(e) = &n.kind else { panic!() };
        &e.iter()
            .find(|(k, _)| matches!(&k.kind, Kind::Scalar{value, ..} if value == key))
            .unwrap()
            .1
    }

    #[test]
    fn resolution_like_yaml_v3() {
        assert_eq!(resolve_plain("0x1F"), Resolved::Int(31));
        assert_eq!(resolve_plain("0o17"), Resolved::Int(15));
        assert_eq!(resolve_plain("017"), Resolved::Int(15));
        assert_eq!(resolve_plain("1_000"), Resolved::Int(1000));
        assert_eq!(resolve_plain("+5"), Resolved::Int(5));
        assert_eq!(resolve_plain("-0b11"), Resolved::Int(-3));
        assert_eq!(resolve_plain("1.5"), Resolved::Float(1.5));
        assert_eq!(resolve_plain("1e3"), Resolved::Float(1000.0));
        assert_eq!(
            resolve_plain("18446744073709551615"),
            Resolved::Uint(u64::MAX)
        );
        assert_eq!(resolve_plain("~"), Resolved::Null);
        assert_eq!(resolve_plain("yes"), Resolved::Str("yes".into()));
        assert_eq!(
            resolve_plain("2001-12-14"),
            Resolved::Timestamp("2001-12-14".into())
        );
        assert_eq!(resolve_plain("1.2.3"), Resolved::Str("1.2.3".into()));
    }

    #[test]
    fn typed_decoding() {
        let n = doc("a: yes\nb: '12'\nc: 1.9\nd: -1\ne: abcdefghijklmn\nf:\ng: [x, ~]\n");
        let mut dec = Decoder::new();
        let mut b = false;
        dec.bool(get(&n, "a"), &mut b);
        assert!(b);
        let mut s = String::new();
        dec.string(get(&n, "b"), &mut s);
        assert_eq!(s, "12");
        let mut i = 0i64;
        dec.int(get(&n, "c"), &mut i, "int");
        assert_eq!(i, 1);
        let mut u = 7u64;
        dec.uint(get(&n, "d"), &mut u, "uint");
        dec.uint(get(&n, "f"), &mut u, "uint");
        assert_eq!(u, 7);
        dec.int(get(&n, "e"), &mut i, "int");
        let mut l = None;
        dec.string_list(get(&n, "g"), &mut l);
        assert_eq!(l, Some(vec!["x".to_string(), String::new()]));
        let err = dec.finish().unwrap_err();
        assert_eq!(
            err,
            "yaml: unmarshal errors:\n  line 4: cannot unmarshal !!int `-1` into uint\n  line 5: cannot unmarshal !!str `abcdefg...` into int"
        );
    }

    fn ints(n: &Node) -> (Vec<(String, i64)>, Result<(), String>) {
        let mut dec = Decoder::new();
        let mut got = Vec::new();
        dec.mapping(n, "T", |d, k, v| {
            let mut i = 0i64;
            d.int(v, &mut i, "int");
            got.push((k.to_string(), i));
            true
        });
        (got, dec.finish())
    }

    #[test]
    fn merge_keys() {
        let n = doc("base: &b {x: 1, y: 2}\nm:\n  <<: *b\n  y: 3\n");
        let (got, r) = ints(get(&n, "m"));
        assert_eq!(got, vec![("y".into(), 3), ("x".into(), 1)]);
        r.unwrap();
        // The first merged mapping wins over the later ones.
        let n = doc("a: &a {x: 1}\nb: &b {x: 2, z: 3}\nm:\n  <<: [*a, *b]\n");
        assert_eq!(ints(get(&n, "m")).0, vec![("x".into(), 1), ("z".into(), 3)]);
    }

    /// yaml.v3 refuses a repeated key in any mapping, known field or not, and then skips the
    /// whole mapping (its other errors are not reported).
    #[test]
    fn duplicate_keys_like_yaml_v3() {
        let n = doc("y: 3\nx: abc\ny: 4\nother: 1\nother: 2\ny: 5\n");
        let (got, r) = ints(&n);
        assert!(got.is_empty(), "{got:?}");
        assert_eq!(
            r.unwrap_err(),
            "yaml: unmarshal errors:\n  line 3: mapping key \"y\" already defined at line 1\n  line 6: mapping key \"y\" already defined at line 1\n  line 6: mapping key \"y\" already defined at line 3\n  line 5: mapping key \"other\" already defined at line 4"
        );
        // Quoted and plain spellings are the same key; nested mappings are checked too.
        let n = doc("m:\n  'a': 1\n  a: 2\n");
        let mut dec = Decoder::new();
        dec.mapping(&n, "T", |d, _, v| {
            d.mapping(v, "U", |_, _, _| true);
            true
        });
        assert_eq!(
            dec.finish().unwrap_err(),
            "yaml: unmarshal errors:\n  line 3: mapping key \"a\" already defined at line 2"
        );
        // Different keys and the same key in different mappings are fine.
        let n = doc("a: {k: 1}\nb: {k: 2}\n");
        assert_eq!(ints(&n).0.len(), 2);
    }

    #[test]
    fn go_quoting() {
        assert_eq!(go_quote("abc"), "\"abc\"");
        assert_eq!(go_quote("a\"b\\c\n\t"), "\"a\\\"b\\\\c\\n\\t\"");
        assert_eq!(go_quote("\u{1}\u{7f}é"), "\"\\x01\\x7fé\"");
        assert_eq!(
            go_quote("\u{85}\u{a0}\u{feff}"),
            "\"\\u0085\\u00a0\\ufeff\""
        );
    }

    #[test]
    fn empty_document_is_none() {
        assert_eq!(parse_first_document("").unwrap(), None);
        assert_eq!(parse_first_document("# only a comment\n").unwrap(), None);
    }
}
