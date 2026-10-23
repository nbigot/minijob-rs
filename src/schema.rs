//! JSON Schema validation with the behavior of `github.com/qri-io/jsonschema` v0.2.1, which
//! the Go server uses for `jobs.jsonSchema`: same keywords (draft 2019-09), evaluation order,
//! messages, instance paths and invalid values, so `validationErrors` are unchanged.
//!
//! Differences (see MIGRATION.md):
//! - the schema is compiled once at startup and shared read-only (qri-io mutates it on first
//!   use, a data race between concurrent requests);
//! - members of an object are evaluated in key order (Go iterates maps in random order, so
//!   the order of several errors varied between requests);
//! - `$ref` is resolved inside the schema file only, including draft-07 `definitions` (qri-io
//!   ignores `definitions`, so such references always failed); remote references are not
//!   downloaded and fail like an unreachable one does in Go;
//! - the details of `format` errors that quote Go library messages are approximations.

use crate::gojson::decode::{Entries, go_text, match_field, members};
use crate::gojson::{fmt_float_v, to_vec};
use regex::Regex;
use serde_json::Value;
use serde_json::value::RawValue;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

/// One validation error (qri-io `KeyError`).
#[derive(Clone, Debug, PartialEq)]
pub struct KeyError {
    /// Instance location, `/` for the root.
    pub property_path: String,
    pub invalid_value: Value,
    pub message: String,
}

/// Keyword names in qri-io registration order (`LoadDraft2019_09`), which breaks ties of the
/// evaluation order.
const KEYWORDS: [&str; 52] = [
    "$schema",
    "$id",
    "description",
    "title",
    "$comment",
    "examples",
    "readOnly",
    "writeOnly",
    "$ref",
    "$recursiveRef",
    "$anchor",
    "$recursiveAnchor",
    "$defs",
    "default",
    "type",
    "enum",
    "const",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "properties",
    "patternProperties",
    "additionalProperties",
    "required",
    "propertyNames",
    "maxProperties",
    "minProperties",
    "dependentSchemas",
    "dependentRequired",
    "unevaluatedProperties",
    "items",
    "additionalItems",
    "maxItems",
    "minItems",
    "uniqueItems",
    "contains",
    "maxContains",
    "minContains",
    "unevaluatedItems",
    "if",
    "then",
    "else",
    "format",
];

fn keyword_order(name: &str) -> (u8, usize) {
    let group = match name {
        "$ref" | "$recursiveRef" => 0,
        "properties" | "maxContains" | "minContains" | "then" | "else" => 2,
        "additionalProperties" | "additionalItems" => 3,
        "unevaluatedProperties" | "unevaluatedItems" => 4,
        _ => 1,
    };
    let insert = KEYWORDS
        .iter()
        .position(|k| *k == name)
        .unwrap_or(usize::MAX);
    (group, insert)
}

type NodeId = usize;

#[derive(Debug)]
enum Kw {
    Ref(String, Option<NodeId>),
    Type(Vec<String>),
    Enum(Vec<(Value, String)>),
    Const(Value),
    MultipleOf(f64),
    Maximum(f64),
    ExclusiveMaximum(f64),
    Minimum(f64),
    ExclusiveMinimum(f64),
    MaxLength(i64),
    MinLength(i64),
    /// The compiled pattern and its source (shown in messages).
    Pattern(Regex, String),
    AllOf(Vec<NodeId>),
    AnyOf(Vec<NodeId>),
    OneOf(Vec<NodeId>),
    Not(NodeId),
    Properties(BTreeMap<String, NodeId>),
    PatternProperties(Vec<(Regex, NodeId)>),
    AdditionalProperties(NodeId),
    Required(Vec<String>),
    PropertyNames(NodeId),
    MaxProperties(i64),
    MinProperties(i64),
    DependentSchemas(BTreeMap<String, NodeId>),
    DependentRequired(BTreeMap<String, Vec<String>>),
    UnevaluatedProperties(NodeId),
    Items {
        single: bool,
        schemas: Vec<NodeId>,
    },
    AdditionalItems(NodeId),
    MaxItems(i64),
    MinItems(i64),
    /// qri-io checks uniqueness whatever the value (`"uniqueItems": false` included).
    UniqueItems,
    Contains(NodeId, String),
    MaxContains(i64),
    MinContains(i64),
    UnevaluatedItems(NodeId),
    If(NodeId),
    Then(NodeId),
    Else(NodeId),
    Format(String),
}

#[derive(Debug)]
enum Node {
    True,
    False,
    /// Keywords in evaluation order; `has_then_else` mirrors qri-io's check in `if`.
    Obj {
        kws: Vec<Kw>,
        has_then_else: bool,
    },
    /// Placeholder while a `$ref` target is compiled (cycles).
    Pending,
    /// A nil `*Schema` (`null` in a list or map of schemas): fails with `schema is nil`.
    Nil,
}

/// A compiled schema. Immutable, `Send + Sync`.
#[derive(Debug)]
pub struct Schema {
    nodes: Vec<Node>,
    root: NodeId,
}

struct Compiler<'a> {
    root_text: &'a str,
    nodes: Vec<Node>,
    by_pointer: HashMap<String, NodeId>,
}

fn json_err(prop: &str, e: impl std::fmt::Display) -> String {
    format!("error unmarshaling {prop} from json: {e}")
}

// Decoding of the keywords with the semantics of Go's `json.Unmarshal` into qri-io's types:
// `null` leaves a value unchanged (zero), other kinds of the wrong type are errors worded as
// Go's `UnmarshalTypeError`.

/// The kind of a JSON value, as Go names it in its errors.
fn go_kind(raw: &RawValue) -> &'static str {
    match raw.get().trim_start().as_bytes().first() {
        Some(b'n') => "null",
        Some(b't' | b'f') => "bool",
        Some(b'"') => "string",
        Some(b'[') => "array",
        Some(b'{') => "object",
        _ => "number",
    }
}

fn type_error(value: &str, go_type: &str) -> String {
    format!("json: cannot unmarshal {value} into Go value of type {go_type}")
}

/// Into a Go string type; `None` for `null`.
fn go_string(raw: &RawValue, go_type: &str) -> Result<Option<String>, String> {
    match go_kind(raw) {
        "null" => Ok(None),
        "string" => serde_json::from_str(raw.get())
            .map(Some)
            .map_err(|e| e.to_string()),
        k => Err(type_error(k, go_type)),
    }
}

/// Into a Go bool type; `None` for `null`.
fn go_bool(raw: &RawValue, go_type: &str) -> Result<Option<bool>, String> {
    match go_kind(raw) {
        "null" => Ok(None),
        "bool" => Ok(Some(raw.get().trim() == "true")),
        k => Err(type_error(k, go_type)),
    }
}

/// Into a Go float64 type: `strconv.ParseFloat` of the literal (`1e400` is an error).
fn go_float(raw: &RawValue, go_type: &str) -> Result<Option<f64>, String> {
    match go_kind(raw) {
        "null" => Ok(None),
        "number" => {
            let lit = raw.get().trim();
            match lit.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok(Some(f)),
                _ => Err(type_error(&format!("number {lit}"), go_type)),
            }
        }
        k => Err(type_error(k, go_type)),
    }
}

/// Into a Go int type: `strconv.ParseInt` of the literal (`-0` is 0; `1.0`, `1e3` and
/// out of range values are errors).
fn go_int(raw: &RawValue, go_type: &str) -> Result<Option<i64>, String> {
    match go_kind(raw) {
        "null" => Ok(None),
        "number" => {
            let lit = raw.get().trim();
            let digits = lit.strip_prefix('-').unwrap_or(lit);
            match lit.parse::<i64>() {
                Ok(i) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                    Ok(Some(i))
                }
                _ => Err(type_error(&format!("number {lit}"), go_type)),
            }
        }
        k => Err(type_error(k, go_type)),
    }
}

/// Into a Go slice type: the elements; `None` for `null`.
fn go_array<'a>(raw: &'a RawValue, go_type: &str) -> Result<Option<Vec<&'a RawValue>>, String> {
    match go_kind(raw) {
        "null" => Ok(None),
        "array" => serde_json::from_str(raw.get())
            .map(Some)
            .map_err(|e| e.to_string()),
        k => Err(type_error(k, go_type)),
    }
}

/// Into a Go map type: the members in document order (duplicates kept: Go decodes each, the
/// last one stays); `None` for `null`.
fn go_object(raw: &RawValue, go_type: &str) -> Result<Option<Entries>, String> {
    match go_kind(raw) {
        "null" => Ok(None),
        "object" => members(raw).map_err(|_| type_error("object", go_type)),
        k => Err(type_error(k, go_type)),
    }
}

/// Into a `[]string` type: `null` elements are `""`; the first wrong element is the error.
fn go_strings(raw: &RawValue, go_type: &str) -> Result<Option<Vec<String>>, String> {
    let Some(items) = go_array(raw, go_type)? else {
        return Ok(None);
    };
    items
        .into_iter()
        .map(|item| go_string(item, "string").map(Option::unwrap_or_default))
        .collect::<Result<_, _>>()
        .map(Some)
}

/// Into an `interface{}`: anything, except a number out of the float64 range.
fn go_any(raw: &RawValue) -> Result<(), String> {
    let text = raw.get().as_bytes();
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'"' => {
                i += 1;
                while i < text.len() && text[i] != b'"' {
                    i += if text[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < text.len()
                    && matches!(text[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    i += 1;
                }
                let lit = String::from_utf8_lossy(&text[start..i]);
                if !lit.parse::<f64>().is_ok_and(f64::is_finite) {
                    return Err(type_error(&format!("number {lit}"), "float64"));
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// Evaluates a JSON pointer (RFC 6901) over raw JSON text.
/// The target is returned as written in the document (qri-io keeps the raw text of its
/// subschemas, which `enum` and `contains` messages quote).
fn eval_pointer<'a>(text: &'a str, pointer: &str) -> Option<&'a str> {
    let mut current: &'a RawValue = serde_json::from_str(text).ok()?;
    if pointer.is_empty() {
        return Some(text);
    }
    for token in pointer.strip_prefix('/')?.split('/') {
        let token = percent_encoding::percent_decode_str(token)
            .decode_utf8_lossy()
            .replace("~1", "/")
            .replace("~0", "~");
        let raw = current.get();
        current = match raw.trim_start().as_bytes().first() {
            // The last duplicate key wins, as when decoding into a map.
            Some(b'{') => *serde_json::from_str::<HashMap<String, &'a RawValue>>(raw)
                .ok()?
                .get(&token)?,
            Some(b'[') => {
                let items: Vec<&'a RawValue> = serde_json::from_str(raw).ok()?;
                *items.get(token.parse::<usize>().ok()?)?
            }
            _ => return None,
        };
    }
    Some(current.get())
}

/// Finds a sub-schema by `$anchor` or by a local `$id` (`#name`).
fn find_anchor(v: &Value, name: &str, path: &mut Vec<String>) -> Option<String> {
    match v {
        Value::Object(m) => {
            let hit = m.get("$anchor").and_then(Value::as_str) == Some(name)
                || m.get("$id").and_then(Value::as_str) == Some(&format!("#{name}"));
            if hit {
                return Some(path.iter().map(|t| format!("/{t}")).collect());
            }
            for (k, child) in m {
                path.push(k.replace('~', "~0").replace('/', "~1"));
                if let Some(p) = find_anchor(child, name, path) {
                    return Some(p);
                }
                path.pop();
            }
            None
        }
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                path.push(i.to_string());
                if let Some(p) = find_anchor(child, name, path) {
                    return Some(p);
                }
                path.pop();
            }
            None
        }
        _ => None,
    }
}

impl Compiler<'_> {
    fn push(&mut self, n: Node) -> NodeId {
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    /// `Schema.UnmarshalJSON`: Go's error is returned as is (each keyword adds its prefix).
    fn compile(&mut self, raw: &RawValue) -> Result<NodeId, String> {
        match go_kind(raw) {
            // `json.Unmarshal(data, &b)` succeeds for a boolean, and for `null` (false).
            "bool" if raw.get().trim() == "true" => return Ok(self.push(Node::True)),
            "bool" | "null" => return Ok(self.push(Node::False)),
            "object" => {}
            k => return Err(type_error(k, "jsonschema._schema")),
        }
        let entries = members(raw).ok().flatten().unwrap_or_default();
        // Decoded into a struct with `$id` first (keys matched without case).
        for (key, value) in &entries {
            if match_field(key, &["$id"]).is_some() && go_string(value, "string").is_err() {
                return Err(format!(
                    "json: cannot unmarshal {} into Go struct field _schema.$id of type string",
                    go_kind(value)
                ));
            }
        }
        // Then into a map: the last duplicate wins.
        let members: BTreeMap<&str, &RawValue> =
            entries.iter().map(|(k, v)| (k.as_str(), &**v)).collect();
        let id = self.push(Node::Pending);
        let mut named: Vec<(&str, Kw)> = Vec::new();
        for (name, raw) in &members {
            if let Some(kw) = self.keyword(name, raw).map_err(|e| json_err(name, e))? {
                named.push((
                    KEYWORDS.iter().find(|k| *k == name).copied().unwrap_or(""),
                    kw,
                ));
            }
        }
        named.sort_by_key(|(name, _)| keyword_order(name));
        let has_then_else = members.contains_key("then") || members.contains_key("else");
        self.nodes[id] = Node::Obj {
            kws: named.into_iter().map(|(_, k)| k).collect(),
            has_then_else,
        };
        Ok(id)
    }

    /// A `*Schema` element of a slice or map: `null` is a nil schema.
    fn compile_ptr(&mut self, raw: &RawValue) -> Result<NodeId, String> {
        if go_kind(raw) == "null" {
            return Ok(self.push(Node::Nil));
        }
        self.compile(raw)
    }

    /// `[]*Schema`.
    fn schema_list(&mut self, raw: &RawValue, go_type: &str) -> Result<Vec<NodeId>, String> {
        go_array(raw, go_type)?
            .unwrap_or_default()
            .into_iter()
            .map(|v| self.compile_ptr(v))
            .collect()
    }

    /// `map[string]*Schema` (`by_value`: `map[string]Schema`, where `null` is the false
    /// schema).
    fn schema_map(
        &mut self,
        raw: &RawValue,
        go_type: &str,
        by_value: bool,
    ) -> Result<BTreeMap<String, NodeId>, String> {
        let mut out = BTreeMap::new();
        for (k, v) in go_object(raw, go_type)?.unwrap_or_default() {
            let id = if by_value {
                self.compile(&v)?
            } else {
                self.compile_ptr(&v)?
            };
            out.insert(k, id);
        }
        Ok(out)
    }

    /// Decodes a keyword as qri-io does; the error is without its `error unmarshaling` prefix.
    fn keyword(&mut self, name: &str, raw: &RawValue) -> Result<Option<Kw>, String> {
        let ty = |t: &str| format!("jsonschema.{t}");
        Ok(Some(match name {
            "$ref" => {
                let r = go_string(raw, "string")?.unwrap_or_default();
                let target = self.resolve(&r);
                Kw::Ref(r, target)
            }
            "$recursiveRef" => {
                go_string(raw, "string")?;
                Kw::Ref("#".into(), self.resolve("#"))
            }
            "type" => {
                let vals = match go_kind(raw) {
                    "string" | "null" => vec![go_string(raw, "string")?.unwrap_or_default()],
                    _ => go_strings(raw, "[]string")?.unwrap_or_default(),
                };
                if let Some(t) = vals.iter().find(|t| {
                    ![
                        "null", "boolean", "object", "array", "number", "string", "integer",
                    ]
                    .contains(&t.as_str())
                }) {
                    return Err(format!("\"{t}\" is not a valid type"));
                }
                Kw::Type(vals)
            }
            "enum" => Kw::Enum(
                go_array(raw, &ty("Enum"))?
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| {
                        (
                            serde_json::from_str(r.get()).unwrap_or(Value::Null),
                            r.get().to_string(),
                        )
                    })
                    .collect(),
            ),
            "const" => Kw::Const(serde_json::from_str(raw.get()).unwrap_or(Value::Null)),
            "multipleOf" => Kw::MultipleOf(go_float(raw, &ty("MultipleOf"))?.unwrap_or(0.0)),
            "maximum" => Kw::Maximum(go_float(raw, &ty("Maximum"))?.unwrap_or(0.0)),
            "exclusiveMaximum" => {
                Kw::ExclusiveMaximum(go_float(raw, &ty("ExclusiveMaximum"))?.unwrap_or(0.0))
            }
            "minimum" => Kw::Minimum(go_float(raw, &ty("Minimum"))?.unwrap_or(0.0)),
            "exclusiveMinimum" => {
                Kw::ExclusiveMinimum(go_float(raw, &ty("ExclusiveMinimum"))?.unwrap_or(0.0))
            }
            "maxLength" => Kw::MaxLength(go_int(raw, &ty("MaxLength"))?.unwrap_or(0)),
            "minLength" => Kw::MinLength(go_int(raw, &ty("MinLength"))?.unwrap_or(0)),
            "pattern" => {
                let s = go_string(raw, "string")?.unwrap_or_default();
                let re = crate::goregex::compile(&s)?;
                Kw::Pattern(re, s)
            }
            "allOf" => Kw::AllOf(self.schema_list(raw, &ty("AllOf"))?),
            "anyOf" => Kw::AnyOf(self.schema_list(raw, &ty("AnyOf"))?),
            "oneOf" => Kw::OneOf(self.schema_list(raw, &ty("OneOf"))?),
            "not" => Kw::Not(self.compile(raw)?),
            "properties" => Kw::Properties(self.schema_map(raw, &ty("Properties"), false)?),
            "patternProperties" => {
                let m = self.schema_map(raw, "map[string]*jsonschema.Schema", false)?;
                let mut list = Vec::new();
                for (k, v) in m {
                    let re = crate::goregex::compile(&k)
                        .map_err(|e| format!("invalid pattern: {k}: {e}"))?;
                    list.push((re, v));
                }
                Kw::PatternProperties(list)
            }
            "additionalProperties" => Kw::AdditionalProperties(self.compile(raw)?),
            "required" => Kw::Required(go_strings(raw, &ty("Required"))?.unwrap_or_default()),
            "propertyNames" => Kw::PropertyNames(self.compile(raw)?),
            "maxProperties" => Kw::MaxProperties(go_int(raw, &ty("MaxProperties"))?.unwrap_or(0)),
            "minProperties" => Kw::MinProperties(go_int(raw, &ty("MinProperties"))?.unwrap_or(0)),
            "dependentSchemas" => {
                Kw::DependentSchemas(self.schema_map(raw, &ty("_dependentSchemas"), true)?)
            }
            "dependentRequired" => {
                let mut deps = BTreeMap::new();
                for (k, v) in go_object(raw, &ty("_dependentRequired"))?.unwrap_or_default() {
                    deps.insert(k, go_strings(&v, "[]string")?.unwrap_or_default());
                }
                Kw::DependentRequired(deps)
            }
            "unevaluatedProperties" => Kw::UnevaluatedProperties(self.compile(raw)?),
            // A schema, else a list of schemas (whose error is the one reported).
            "items" => match self.compile(raw) {
                Ok(id) => Kw::Items {
                    single: true,
                    schemas: vec![id],
                },
                Err(_) => Kw::Items {
                    single: false,
                    schemas: self.schema_list(raw, "[]*jsonschema.Schema")?,
                },
            },
            "additionalItems" => Kw::AdditionalItems(self.compile(raw)?),
            "maxItems" => Kw::MaxItems(go_int(raw, &ty("MaxItems"))?.unwrap_or(0)),
            "minItems" => Kw::MinItems(go_int(raw, &ty("MinItems"))?.unwrap_or(0)),
            "uniqueItems" => {
                go_bool(raw, &ty("UniqueItems"))?;
                Kw::UniqueItems
            }
            "contains" => Kw::Contains(self.compile(raw)?, raw.get().to_string()),
            "maxContains" => Kw::MaxContains(go_int(raw, &ty("MaxContains"))?.unwrap_or(0)),
            "minContains" => Kw::MinContains(go_int(raw, &ty("MinContains"))?.unwrap_or(0)),
            "unevaluatedItems" => Kw::UnevaluatedItems(self.compile(raw)?),
            "if" => Kw::If(self.compile(raw)?),
            "then" => Kw::Then(self.compile(raw)?),
            "else" => Kw::Else(self.compile(raw)?),
            "format" => Kw::Format(go_string(raw, &ty("Format"))?.unwrap_or_default()),
            // Reached through `$ref`; decoded for the errors only.
            "$defs" => {
                self.schema_map(raw, &ty("Defs"), false)?;
                return Ok(None);
            }
            "$recursiveAnchor" => {
                self.compile(raw)?;
                return Ok(None);
            }
            "$schema" | "$id" | "$anchor" | "$comment" | "title" | "description" => {
                let t = match name {
                    "$schema" => "SchemaURI",
                    "$id" => "ID",
                    "$anchor" => "Anchor",
                    "$comment" => "Comment",
                    "title" => "Title",
                    _ => "Description",
                };
                go_string(raw, &ty(t))?;
                return Ok(None);
            }
            "readOnly" | "writeOnly" => {
                let t = if name == "readOnly" {
                    "ReadOnly"
                } else {
                    "WriteOnly"
                };
                go_bool(raw, &ty(t))?;
                return Ok(None);
            }
            "examples" => {
                for item in go_array(raw, &ty("Examples"))?.unwrap_or_default() {
                    go_any(item)?;
                }
                return Ok(None);
            }
            "default" => {
                go_any(raw)?;
                return Ok(None);
            }
            // Unsupported keywords (`definitions`, `dependencies`, ...) and unknown members
            // are not decoded.
            _ => return Ok(None),
        }))
    }

    /// Resolves a reference inside the document; `None` when it points elsewhere, or to a
    /// subschema that does not compile (its error is reported by its own keyword, or not at
    /// all where qri-io does not decode it, as under `definitions`).
    fn resolve(&mut self, reference: &str) -> Option<NodeId> {
        let fragment = reference.strip_prefix('#')?;
        let pointer = if fragment.is_empty() || fragment.starts_with('/') {
            fragment.to_string()
        } else {
            let root: Value = serde_json::from_str(self.root_text).unwrap_or(Value::Null);
            find_anchor(&root, fragment, &mut Vec::new())?
        };
        if let Some(id) = self.by_pointer.get(&pointer) {
            return Some(*id);
        }
        let text = eval_pointer(self.root_text, &pointer)?;
        if pointer.is_empty() {
            // The root itself: its id is 0 once compiled.
            return Some(0);
        }
        let raw: &RawValue = serde_json::from_str(text).ok()?;
        // Reserve the id before compiling to support recursive references.
        let reserved = self.nodes.len();
        self.by_pointer.insert(pointer.clone(), reserved);
        match self.compile(raw) {
            Ok(id) => {
                if id != reserved {
                    self.by_pointer.insert(pointer, id);
                }
                Some(id)
            }
            Err(_) => {
                self.by_pointer.remove(&pointer);
                None
            }
        }
    }
}

impl Schema {
    /// Compiles a schema document (`Schema.UnmarshalJSON`).
    pub fn compile(bytes: &[u8]) -> Result<Schema, String> {
        // Invalid UTF-8 and lone surrogates read as U+FFFD, as in Go.
        let text = go_text(bytes).into_owned();
        let raw: &RawValue = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let mut c = Compiler {
            root_text: raw.get(),
            nodes: Vec::new(),
            by_pointer: HashMap::new(),
        };
        let root = c.compile(raw)?;
        Ok(Schema {
            nodes: c.nodes,
            root,
        })
    }

    /// Validates a decoded document (`Schema.Validate`). The errors are sorted by path (byte
    /// order, stable), as Go sorts them.
    pub fn validate(&self, data: &Value) -> Vec<KeyError> {
        let st = St::new();
        self.validate_node(Some(self.root), &st, data);
        let mut errs = st.errs.take();
        errs.sort_by(|a, b| a.property_path.cmp(&b.property_path));
        errs
    }

    fn validate_node(&self, id: Option<NodeId>, st: &St, data: &Value) {
        let Some(id) = id else {
            st.add_error(data, "schema is nil".to_string());
            return;
        };
        match &self.nodes[id] {
            Node::True | Node::Pending => {}
            Node::Nil => st.add_error(data, "schema is nil".to_string()),
            Node::False => st.add_error(data, "schema is always false".to_string()),
            Node::Obj { kws, has_then_else } => {
                for kw in kws {
                    self.keyword(kw, *has_then_else, st, data);
                }
            }
        }
    }

    fn is_false(&self, id: NodeId) -> bool {
        matches!(self.nodes[id], Node::False)
    }

    fn keyword(&self, kw: &Kw, has_then_else: bool, st: &St, data: &Value) {
        match kw {
            Kw::Ref(reference, target) => {
                if target.is_none() {
                    st.add_error(
                        data,
                        format!("failed to resolve schema for ref {reference}"),
                    );
                }
                let mut sub = st.sub();
                sub.clear();
                self.validate_node(*target, &sub, data);
                st.update_from(&sub);
            }
            Kw::Type(vals) => {
                let jt = data_type(data);
                for t in vals {
                    if jt == *t || (jt == "integer" && t == "number") {
                        return;
                    }
                    if jt == "string"
                        && t == "boolean"
                        && parse_go_bool(data.as_str().unwrap_or(""))
                    {
                        return;
                    }
                }
                if vals.len() == 1 {
                    st.add_error(data, format!("type should be {}, got {jt}", vals[0]));
                } else {
                    st.add_error(
                        data,
                        format!("type should be one of: {}, got {jt}", vals.join(",")),
                    );
                }
            }
            Kw::Enum(items) => {
                if items.iter().any(|(v, _)| go_deep_equal(v, data)) {
                    return;
                }
                let list: Vec<&str> = items.iter().map(|(_, raw)| raw.as_str()).collect();
                st.add_error(data, format!("should be one of [{}]", list.join(", ")));
            }
            Kw::Const(c) => {
                if !go_deep_equal(c, data) {
                    st.add_error(data, format!("must equal {}", invalid_value_string(c)));
                }
            }
            Kw::MultipleOf(m) => {
                if let Some(n) = data.as_f64() {
                    let div = n / m;
                    if go_int_of(div) as f64 != div {
                        st.add_error(data, format!("must be a multiple of {}", fv(*m)));
                    }
                }
            }
            Kw::Maximum(m) => {
                if let Some(n) = data.as_f64().filter(|n| n > m) {
                    let _ = n;
                    st.add_error(data, format!("must be less than or equal to {}", fv(*m)));
                }
            }
            Kw::ExclusiveMaximum(m) => {
                if let Some(n) = data.as_f64().filter(|n| n >= m) {
                    st.add_error(data, format!("{} must be less than {}", fv(n), fv(*m)));
                }
            }
            Kw::Minimum(m) => {
                if data.as_f64().is_some_and(|n| n < *m) {
                    st.add_error(data, format!("must be greater than or equal to {}", fv(*m)));
                }
            }
            Kw::ExclusiveMinimum(m) => {
                if let Some(n) = data.as_f64().filter(|n| n <= m) {
                    st.add_error(data, format!("{} must be greater than {}", fv(n), fv(*m)));
                }
            }
            Kw::MaxLength(m) => {
                if let Some(s) = data.as_str()
                    && s.chars().count() as i64 > *m
                {
                    st.add_error(data, format!("max length of {m} characters exceeded: {s}"));
                }
            }
            Kw::MinLength(m) => {
                if let Some(s) = data.as_str()
                    && (s.chars().count() as i64) < *m
                {
                    st.add_error(data, format!("min length of {m} characters required: {s}"));
                }
            }
            Kw::Pattern(re, source) => {
                if let Some(s) = data.as_str()
                    && !re.is_match(s)
                {
                    st.add_error(
                        data,
                        format!("regexp pattern {source} mismatch on string: {s}"),
                    );
                }
            }
            Kw::AllOf(list) => {
                let mut copy = st.sub();
                copy.clear();
                let mut invalid = false;
                for id in list {
                    let mut sub = st.sub();
                    sub.clear();
                    sub.errs = Rc::new(RefCell::new(Vec::new()));
                    self.validate_node(Some(*id), &sub, data);
                    st.add_sub_errors(&sub);
                    copy.update_from(&sub);
                    if !sub.is_valid() {
                        invalid = true;
                    }
                }
                if !invalid {
                    st.update_from(&copy);
                }
            }
            Kw::AnyOf(list) => {
                for id in list {
                    let mut sub = st.sub();
                    sub.clear();
                    sub.errs = Rc::new(RefCell::new(Vec::new()));
                    self.validate_node(Some(*id), &sub, data);
                    if sub.is_valid() {
                        st.update_from(&sub);
                        return;
                    }
                }
                st.add_error(
                    data,
                    "did Not match any specified AnyOf schemas".to_string(),
                );
            }
            Kw::OneOf(list) => {
                let mut matched = false;
                let mut copy = st.sub();
                copy.clear();
                for id in list {
                    let mut sub = st.sub();
                    sub.clear();
                    sub.errs = Rc::new(RefCell::new(Vec::new()));
                    self.validate_node(Some(*id), &sub, data);
                    copy.update_from(&sub);
                    if sub.is_valid() {
                        if matched {
                            st.add_error(
                                data,
                                "matched more than one specified OneOf schemas".to_string(),
                            );
                            return;
                        }
                        matched = true;
                    }
                }
                if matched {
                    st.update_from(&copy);
                } else {
                    st.add_error(
                        data,
                        "did not match any of the specified OneOf schemas".to_string(),
                    );
                }
            }
            Kw::Not(id) => {
                let mut sub = st.sub();
                sub.errs = Rc::new(RefCell::new(Vec::new()));
                self.validate_node(Some(*id), &sub, data);
                if sub.is_valid() {
                    st.add_error(
                        data,
                        "result was valid, ('not') expected invalid".to_string(),
                    );
                }
            }
            Kw::Properties(props) => {
                if let Value::Object(obj) = data {
                    let mut sub = st.sub();
                    for (key, id) in props {
                        if let Some(v) = obj.get(key) {
                            st.set_evaluated_key(key);
                            sub.clear();
                            sub.instance = st.descend(key);
                            sub.errs = Rc::new(RefCell::new(Vec::new()));
                            self.validate_node(Some(*id), &sub, v);
                            st.add_sub_errors(&sub);
                            if sub.is_valid() {
                                st.update_from(&sub);
                            }
                        }
                    }
                }
            }
            Kw::PatternProperties(list) => {
                if let Value::Object(obj) = data {
                    for (key, val) in obj {
                        for (re, id) in list {
                            if re.is_match(key) {
                                st.set_evaluated_key(key);
                                let mut sub = st.sub();
                                sub.instance = st.descend(key);
                                sub.errs = Rc::new(RefCell::new(Vec::new()));
                                self.validate_node(Some(*id), &sub, val);
                                st.add_sub_errors(&sub);
                                if sub.is_valid() {
                                    st.update_from(&sub);
                                }
                            }
                        }
                    }
                }
            }
            Kw::AdditionalProperties(id) => {
                if let Value::Object(obj) = data {
                    let mut sub = st.sub();
                    sub.clear();
                    for (key, val) in obj {
                        if st.is_locally_evaluated(key) {
                            continue;
                        }
                        if self.is_false(*id) {
                            st.add_error(data, "additional properties are not allowed".to_string());
                            return;
                        }
                        st.set_evaluated_key(key);
                        sub.clear();
                        sub.instance = st.descend(key);
                        self.validate_node(Some(*id), &sub, val);
                        st.update_from(&sub);
                    }
                }
            }
            Kw::Required(keys) => {
                if let Value::Object(obj) = data {
                    for k in keys {
                        if !obj.contains_key(k) {
                            st.add_error(data, format!("\"{k}\" value is required"));
                        }
                    }
                }
            }
            Kw::PropertyNames(id) => {
                if let Value::Object(obj) = data {
                    for key in obj.keys() {
                        let mut sub = st.sub();
                        sub.instance = sub.descend(key);
                        self.validate_node(Some(*id), &sub, &Value::String(key.clone()));
                    }
                }
            }
            Kw::MaxProperties(m) => {
                if let Value::Object(obj) = data
                    && obj.len() as i64 > *m
                {
                    st.add_error(
                        data,
                        format!("{} object Properties exceed {m} maximum", obj.len()),
                    );
                }
            }
            Kw::MinProperties(m) => {
                if let Value::Object(obj) = data
                    && (obj.len() as i64) < *m
                {
                    st.add_error(
                        data,
                        format!("{} object Properties below {m} minimum", obj.len()),
                    );
                }
            }
            Kw::DependentSchemas(map) => {
                for (prop, id) in map {
                    let sub = st.sub();
                    if let Value::Object(obj) = data
                        && obj.contains_key(prop)
                    {
                        let inner = sub.sub();
                        self.validate_node(Some(*id), &inner, data);
                    }
                }
            }
            Kw::DependentRequired(map) => {
                if let Value::Object(obj) = data {
                    for (prop, deps) in map {
                        if obj.get(prop).is_none_or(Value::is_null) {
                            continue;
                        }
                        for d in deps {
                            if obj.get(d).is_none_or(Value::is_null) {
                                st.add_error(data, format!("\"{d}\" property is required"));
                            }
                        }
                    }
                }
            }
            Kw::UnevaluatedProperties(id) => {
                if let Value::Object(obj) = data {
                    let mut sub = st.sub();
                    sub.clear();
                    for (key, val) in obj {
                        if st.is_evaluated(key) {
                            continue;
                        }
                        if self.is_false(*id) {
                            st.add_error(
                                data,
                                "unevaluated properties are not allowed".to_string(),
                            );
                            return;
                        }
                        sub.instance = st.descend(key);
                        self.validate_node(Some(*id), &sub, val);
                    }
                }
            }
            Kw::Items { single, schemas } => {
                if let Value::Array(arr) = data {
                    if *single {
                        let mut sub = st.sub();
                        for (i, elem) in arr.iter().enumerate() {
                            sub.clear();
                            sub.instance = st.descend(&i.to_string());
                            self.validate_node(Some(schemas[0]), &sub, elem);
                            sub.set_evaluated_index(i as i64);
                            st.update_from(&sub);
                        }
                    } else {
                        let mut sub = st.sub();
                        for (i, id) in schemas.iter().enumerate() {
                            if i < arr.len() {
                                sub.clear();
                                sub.instance = st.descend(&i.to_string());
                                self.validate_node(Some(*id), &sub, &arr[i]);
                                sub.set_evaluated_index(i as i64);
                                st.update_from(&sub);
                            }
                        }
                    }
                }
            }
            Kw::AdditionalItems(id) => {
                if let Value::Array(arr) = data {
                    let last = st.last_index.get();
                    if last > -1 && last < arr.len() as i64 {
                        for (i, item) in arr.iter().enumerate().skip((last + 1) as usize) {
                            if self.is_false(*id) {
                                st.add_error(data, "additional items are not allowed".to_string());
                                return;
                            }
                            let mut sub = st.sub();
                            sub.clear();
                            sub.set_evaluated_index(i as i64);
                            sub.instance = sub.descend(&i.to_string());
                            self.validate_node(Some(*id), &sub, item);
                            st.update_from(&sub);
                        }
                    }
                }
            }
            Kw::MaxItems(m) => {
                if let Value::Array(arr) = data
                    && arr.len() as i64 > *m
                {
                    st.add_error(data, format!("array length {} exceeds {m} max", arr.len()));
                }
            }
            Kw::MinItems(m) => {
                if let Value::Array(arr) = data
                    && (arr.len() as i64) < *m
                {
                    st.add_error(
                        data,
                        format!("array length {} below {m} minimum items", arr.len()),
                    );
                }
            }
            Kw::UniqueItems => {
                if let Value::Array(arr) = data {
                    for (i, elem) in arr.iter().enumerate() {
                        if arr[..i].iter().any(|f| go_deep_equal(f, elem)) {
                            st.add_error(
                                data,
                                format!(
                                    "array items must be unique. duplicated entry: {}",
                                    crate::gojson::fmt_v(elem)
                                ),
                            );
                            return;
                        }
                    }
                }
            }
            Kw::Contains(id, raw) => {
                if let Value::Array(arr) = data {
                    let mut count = 0;
                    let mut sub = st.sub();
                    sub.clear();
                    for (i, elem) in arr.iter().enumerate() {
                        sub.clear();
                        sub.instance = st.descend(&i.to_string());
                        sub.errs = Rc::new(RefCell::new(Vec::new()));
                        self.validate_node(Some(*id), &sub, elem);
                        if sub.is_valid() {
                            count += 1;
                        }
                    }
                    if count > 0 {
                        st.contains_count.set(Some(count));
                    } else {
                        st.add_error(data, format!("must contain at least one of: {raw}"));
                    }
                }
            }
            Kw::MaxContains(m) => {
                if let (Value::Array(arr), Some(c)) = (data, st.contains_count.get())
                    && c as i64 > *m
                {
                    st.add_error(
                        data,
                        format!("contained items {} exceeds {m} max", arr.len()),
                    );
                }
            }
            Kw::MinContains(m) => {
                if let (Value::Array(arr), Some(c)) = (data, st.contains_count.get())
                    && (c as i64) < *m
                {
                    st.add_error(
                        data,
                        format!("contained items {} bellow {m} min", arr.len()),
                    );
                }
            }
            Kw::UnevaluatedItems(id) => {
                if let Value::Array(arr) = data {
                    let last = st.last_index.get();
                    if last < arr.len() as i64 {
                        for (i, item) in arr.iter().enumerate().skip((last + 1).max(0) as usize) {
                            if self.is_false(*id) {
                                st.add_error(data, "unevaluated items are not allowed".to_string());
                                return;
                            }
                            let mut sub = st.sub();
                            sub.clear();
                            sub.instance = sub.descend(&i.to_string());
                            self.validate_node(Some(*id), &sub, item);
                        }
                    }
                }
            }
            Kw::If(id) => {
                if !has_then_else {
                    return;
                }
                let mut sub = st.sub();
                sub.clear();
                sub.errs = Rc::new(RefCell::new(Vec::new()));
                self.validate_node(Some(*id), &sub, data);
                st.if_result.set(Some(sub.is_valid()));
            }
            Kw::Then(id) => {
                if st.if_result.get() == Some(true) {
                    let sub = st.sub();
                    self.validate_node(Some(*id), &sub, data);
                    st.update_from(&sub);
                }
            }
            Kw::Else(id) => {
                if st.if_result.get() == Some(false) {
                    let sub = st.sub();
                    self.validate_node(Some(*id), &sub, data);
                }
            }
            Kw::Format(f) => {
                if let Some(s) = data.as_str()
                    && let Err(e) = check_format(f, s)
                {
                    st.add_error(data, format!("invalid {f}: {e}"));
                }
            }
        }
    }
}

/// Validation state (`ValidationState`): sets shared with sub-states until cleared, as in
/// qri-io, where they are pointers.
#[derive(Clone)]
struct St {
    instance: Vec<String>,
    evaluated: Rc<RefCell<HashSet<String>>>,
    local_evaluated: Rc<RefCell<HashSet<String>>>,
    last_index: Rc<std::cell::Cell<i64>>,
    local_last_index: Rc<std::cell::Cell<i64>>,
    if_result: Rc<std::cell::Cell<Option<bool>>>,
    contains_count: Rc<std::cell::Cell<Option<usize>>>,
    errs: Rc<RefCell<Vec<KeyError>>>,
}

impl St {
    fn new() -> St {
        St {
            instance: Vec::new(),
            evaluated: Rc::default(),
            local_evaluated: Rc::default(),
            last_index: Rc::new(std::cell::Cell::new(-1)),
            local_last_index: Rc::new(std::cell::Cell::new(-1)),
            if_result: Rc::default(),
            contains_count: Rc::default(),
            errs: Rc::default(),
        }
    }

    /// `NewSubState`: shares the sets and the error list, copies the indexes, fresh `Misc`.
    fn sub(&self) -> St {
        St {
            instance: self.instance.clone(),
            evaluated: self.evaluated.clone(),
            local_evaluated: self.local_evaluated.clone(),
            last_index: Rc::new(std::cell::Cell::new(self.last_index.get())),
            local_last_index: Rc::new(std::cell::Cell::new(self.local_last_index.get())),
            if_result: Rc::default(),
            contains_count: Rc::default(),
            errs: self.errs.clone(),
        }
    }

    /// `ClearState`.
    fn clear(&mut self) {
        self.evaluated = Rc::default();
        self.local_evaluated = Rc::default();
        self.if_result = Rc::default();
        self.contains_count = Rc::default();
    }

    fn descend(&self, token: &str) -> Vec<String> {
        let mut p = self.instance.clone();
        p.push(token.to_string());
        p
    }

    fn add_error(&self, data: &Value, message: String) {
        let mut path: String = self
            .instance
            .iter()
            .map(|t| format!("/{}", t.replace('~', "~0").replace('/', "~1")))
            .collect();
        if path.is_empty() {
            path = "/".to_string();
        }
        self.errs.borrow_mut().push(KeyError {
            property_path: path,
            invalid_value: data.clone(),
            message,
        });
    }

    fn add_sub_errors(&self, sub: &St) {
        if !Rc::ptr_eq(&self.errs, &sub.errs) {
            let errs = sub.errs.borrow().clone();
            self.errs.borrow_mut().extend(errs);
        }
    }

    fn is_valid(&self) -> bool {
        self.errs.borrow().is_empty()
    }

    fn set_evaluated_key(&self, key: &str) {
        self.evaluated.borrow_mut().insert(key.to_string());
        self.local_evaluated.borrow_mut().insert(key.to_string());
    }

    fn is_evaluated(&self, key: &str) -> bool {
        self.evaluated.borrow().contains(key)
    }

    fn is_locally_evaluated(&self, key: &str) -> bool {
        self.local_evaluated.borrow().contains(key)
    }

    fn set_evaluated_index(&self, i: i64) {
        self.last_index.set(i);
        self.local_last_index.set(i);
    }

    /// `UpdateEvaluatedPropsAndItems` (including qri-io's comparison of the local index with
    /// the non-local one).
    fn update_from(&self, sub: &St) {
        if !Rc::ptr_eq(&self.evaluated, &sub.evaluated) {
            let keys: Vec<String> = sub.evaluated.borrow().iter().cloned().collect();
            self.evaluated.borrow_mut().extend(keys);
        }
        if !Rc::ptr_eq(&self.local_evaluated, &sub.local_evaluated) {
            let keys: Vec<String> = sub.local_evaluated.borrow().iter().cloned().collect();
            self.local_evaluated.borrow_mut().extend(keys);
        }
        if sub.last_index.get() > self.last_index.get() {
            self.last_index.set(sub.last_index.get());
        }
        if sub.local_last_index.get() > self.last_index.get() {
            self.last_index.set(sub.local_last_index.get());
        }
    }
}

fn fv(f: f64) -> String {
    let mut s = String::new();
    fmt_float_v(f, &mut s);
    s
}

/// Go `int(f)` for a float64 on amd64 (out of range → minimum int).
fn go_int_of(f: f64) -> i64 {
    if f.is_nan() || !(i64::MIN as f64..i64::MAX as f64).contains(&f) {
        i64::MIN
    } else {
        f as i64
    }
}

/// qri-io `DataType` on a value decoded by Go (numbers are float64).
fn data_type(v: &Value) -> String {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            if go_int_of(f) as f64 == f {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
    .to_string()
}

/// Go `strconv.ParseBool` succeeds.
fn parse_go_bool(s: &str) -> bool {
    matches!(
        s,
        "1" | "t" | "T" | "TRUE" | "true" | "True" | "0" | "f" | "F" | "FALSE" | "false" | "False"
    )
}

/// `reflect.DeepEqual` on values decoded by Go (numbers compared as float64).
fn go_deep_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| go_deep_equal(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| go_deep_equal(v, w)))
        }
        _ => a == b,
    }
}

/// `InvalidValueString`: Go JSON, truncated to 20 bytes followed by `...`.
fn invalid_value_string(v: &Value) -> String {
    let mut bt = to_vec(v);
    if bt.len() > 20 {
        bt.truncate(20);
        bt.extend_from_slice(b"...");
    }
    String::from_utf8_lossy(&bt).into_owned()
}

fn hostname_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([a-zA-Z0-9]|[a-zA-Z0-9][a-zA-Z0-9\-]{0,61}[a-zA-Z0-9])(\.([a-zA-Z0-9]|[a-zA-Z0-9][a-zA-Z0-9\-]{0,61}[a-zA-Z0-9]))*$")
            .expect("valid regex")
    })
}

/// `quote` of Go's `time` package: bytes of non-ASCII and control characters as `\xNN`.
fn time_quote(s: &[u8]) -> String {
    let mut out = String::from("\"");
    for chunk in s.utf8_chunks() {
        for c in chunk.valid().chars() {
            if !c.is_ascii() || c < ' ' {
                for b in c.encode_utf8(&mut [0; 4]).bytes() {
                    out.push_str(&format!("\\x{b:02x}"));
                }
            } else {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out.push('"');
    out
}

/// Go `strings.ToUpper` (one-to-one mappings only, like `unicode.ToUpper`).
fn go_to_upper(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut up = c.to_uppercase();
            match (up.next(), up.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        })
        .collect()
}

/// Go `time.Parse(time.RFC3339, value)`: the generic parser of `time/format.go` on the layout
/// `2006-01-02T15:04:05Z07:00` (the RFC 3339 fast path accepts a subset of it), with its error
/// messages. Unlike RFC 3339, Go accepts a one-digit hour, a comma before the fraction and
/// zone offsets up to `+24:60`.
fn parse_rfc3339(value: &[u8]) -> Result<(), String> {
    const LAYOUT: &[u8] = b"2006-01-02T15:04:05Z07:00";
    let parse_error = |layout_elem: &[u8], value_elem: &[u8]| {
        format!(
            "parsing time {} as {}: cannot parse {} as {}",
            time_quote(value),
            time_quote(LAYOUT),
            time_quote(value_elem),
            time_quote(layout_elem)
        )
    };
    let range_error =
        |what: &str| format!("parsing time {}: {what} out of range", time_quote(value));
    let digit = |s: &[u8], i: usize| s.get(i).is_some_and(u8::is_ascii_digit);
    // `getnum`: one or two digits (`fixed`: exactly two).
    let getnum = |s: &[u8], fixed: bool| -> Option<(i64, usize)> {
        if !digit(s, 0) {
            return None;
        }
        if !digit(s, 1) {
            return (!fixed).then(|| (i64::from(s[0] - b'0'), 1));
        }
        Some((i64::from(s[0] - b'0') * 10 + i64::from(s[1] - b'0'), 2))
    };
    let mut v = value;
    let (mut year, mut month, mut day) = (0i64, 1i64, 1i64);
    // (literal before the element, element of the layout)
    let chunks: [(&[u8], &[u8]); 7] = [
        (b"", b"2006"),
        (b"-", b"01"),
        (b"-", b"02"),
        (b"T", b"15"),
        (b":", b"04"),
        (b":", b"05"),
        (b"", b"Z07:00"),
    ];
    for (prefix, elem) in chunks {
        if !prefix.is_empty() {
            if !v.starts_with(prefix) {
                return Err(parse_error(prefix, v));
            }
            v = &v[prefix.len()..];
        }
        let hold = v;
        let mut range = None;
        let ok = match elem {
            b"2006" => {
                // Four bytes, the first a digit; `atoi` refuses anything else.
                let ok = v.len() >= 4 && v[..4].iter().all(u8::is_ascii_digit);
                if ok {
                    year = v[..4].iter().fold(0, |y, d| y * 10 + i64::from(d - b'0'));
                    v = &v[4..];
                }
                ok
            }
            b"01" | b"02" | b"04" => match getnum(v, true) {
                Some((n, len)) => {
                    v = &v[len..];
                    match elem {
                        b"01" => {
                            month = n;
                            if !(1..=12).contains(&n) {
                                range = Some("month");
                            }
                        }
                        b"02" => day = n,
                        _ if n >= 60 => range = Some("minute"),
                        _ => {}
                    }
                    true
                }
                None => false,
            },
            b"15" => match getnum(v, false) {
                Some((n, len)) => {
                    v = &v[len..];
                    if n >= 24 {
                        range = Some("hour");
                    }
                    true
                }
                None => false,
            },
            b"05" => match getnum(v, true) {
                Some((n, len)) => {
                    v = &v[len..];
                    if n >= 60 {
                        range = Some("second");
                    } else if v.len() >= 2 && matches!(v[0], b'.' | b',') && digit(v, 1) {
                        // A fraction without a fraction in the layout: any number of digits.
                        let n = 2 + v[2..].iter().take_while(|c| c.is_ascii_digit()).count();
                        v = &v[n..];
                    }
                    true
                }
                None => false,
            },
            _ => {
                if v.first() == Some(&b'Z') {
                    v = &v[1..];
                    true
                } else if v.len() < 6 || v[3] != b':' {
                    false
                } else {
                    let (sign, hour, min) = (v[0], &v[1..3], &v[4..6]);
                    v = &v[6..];
                    let hr = getnum(hour, true);
                    let mm = hr.and_then(|_| getnum(min, true));
                    let (h, m) = (hr.map_or(0, |x| x.0), mm.map_or(0, |x| x.0));
                    if h > 24 {
                        range = Some("time zone offset hour");
                    }
                    if m > 60 {
                        range = Some("time zone offset minute");
                    }
                    mm.is_some() && matches!(sign, b'+' | b'-')
                }
            }
        };
        if let Some(what) = range {
            return Err(range_error(what));
        }
        if !ok {
            return Err(parse_error(elem, hold));
        }
    }
    if !v.is_empty() {
        return Err(format!(
            "parsing time {}: extra text: {}",
            time_quote(value),
            time_quote(v)
        ));
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day < 1 || day > days {
        return Err(range_error("day"));
    }
    Ok(())
}

/// `isValidURIRef`: Go `url.Parse` succeeds and there is no backslash.
fn check_uri_ref(s: &str) -> Result<(), String> {
    crate::gourl::parse(s).map_err(|e| format!("uri incorrectly Formatted: {e}"))?;
    if s.contains('\\') {
        return Err("invalid uri".to_string());
    }
    Ok(())
}

fn check_json_pointer(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Ok(());
    }
    if !p.starts_with('/') {
        return Err("non-empty references must begin with a '/' character".to_string());
    }
    let rest = &p[1..];
    let b = rest.as_bytes();
    for (i, c) in b.iter().enumerate() {
        if *c == b'~' && !matches!(b.get(i + 1), Some(b'0') | Some(b'1')) {
            return Err("unescaped tilda error".to_string());
        }
    }
    Ok(())
}

/// `format` checks of qri-io (`keywords_optional.go`).
fn check_format(format: &str, s: &str) -> Result<(), String> {
    let datetime = |dt: &str| {
        parse_rfc3339(go_to_upper(dt).as_bytes())
            .map_err(|e| format!("date-time incorrectly Formatted: {e}"))
    };
    match format {
        "date-time" => datetime(s),
        "date" => datetime(&format!("{s}T08:30:06.283185Z")),
        "time" => datetime(&format!("1963-06-19T{s}")),
        "email" | "idn-email" => crate::gomail::parse_address(s)
            .map_err(|e| format!("email address incorrectly Formatted: {e}")),
        "hostname" => {
            if hostname_re().is_match(s) && s.len() <= 255 {
                Ok(())
            } else {
                Err("invalid hostname string".to_string())
            }
        }
        "idn-hostname" => {
            if s.len() > 255 {
                return Err("invalid idn hostname string".to_string());
            }
            const BAD: [char; 59] = [
                '\u{20}', '\u{2D}', '\u{A2}', '\u{A3}', '\u{A4}', '\u{A5}', '\u{34F}', '\u{640}',
                '\u{7FA}', '\u{180B}', '\u{180C}', '\u{180D}', '\u{200B}', '\u{2060}', '\u{2104}',
                '\u{2108}', '\u{2114}', '\u{2117}', '\u{2118}', '\u{211E}', '\u{211F}', '\u{2123}',
                '\u{2125}', '\u{2282}', '\u{2283}', '\u{2284}', '\u{2285}', '\u{2286}', '\u{2287}',
                '\u{2288}', '\u{2616}', '\u{2617}', '\u{2619}', '\u{262F}', '\u{2638}', '\u{266C}',
                '\u{266D}', '\u{266F}', '\u{2752}', '\u{2756}', '\u{2758}', '\u{275E}', '\u{2761}',
                '\u{2775}', '\u{2794}', '\u{2798}', '\u{27AF}', '\u{27B1}', '\u{27BE}', '\u{3004}',
                '\u{3012}', '\u{3013}', '\u{3020}', '\u{302E}', '\u{302F}', '\u{3031}', '\u{3032}',
                '\u{3035}', '\u{303B}',
            ];
            match s
                .chars()
                .find(|c| BAD.contains(c) || *c == '\u{3164}' || *c == '\u{FFA0}')
            {
                Some(c) => Err(format!(
                    "invalid hostname: contains illegal character U+{:04X} '{c}'",
                    c as u32
                )),
                None => Ok(()),
            }
        }
        "ipv4" => {
            if s.contains('.') && s.parse::<std::net::IpAddr>().is_ok() {
                Ok(())
            } else {
                Err("invalid IPv4 address".to_string())
            }
        }
        "ipv6" => {
            if s.contains(':') && s.parse::<std::net::IpAddr>().is_ok() {
                Ok(())
            } else {
                // Go's message says IPv4 here too.
                Err("invalid IPv4 address".to_string())
            }
        }
        "iri-reference" | "uri-reference" => check_uri_ref(s),
        "iri" | "uri" => {
            // `isValidURI`: no backslash check here.
            crate::gourl::parse(s).map_err(|e| format!("uri incorrectly Formatted: {e}"))?;
            match s.find(':') {
                Some(i) if i > 0 => Ok(()),
                _ => Err("uri missing scheme prefix".to_string()),
            }
        }
        "uri-template" => {
            let re = Regex::new(r"\{[^\{\}\\]*\}").expect("valid regex");
            let replaced = re.replace_all(s, "aaa");
            if replaced.contains('{') || replaced.contains('}') {
                Err("invalid uri template".to_string())
            } else {
                check_uri_ref(&replaced)
            }
        }
        "json-pointer" => check_json_pointer(s),
        "relative-json-pointer" => {
            let mut parts: Vec<&str> = s.split('/').collect();
            if parts.len() == 1 {
                parts = s.split('#').collect();
            }
            // `strconv.Atoi(parts[0])`: optional sign, decimal digits, an int64; then `i >= 0`.
            let digits = parts[0].strip_prefix(['+', '-']).unwrap_or(parts[0]);
            let atoi = (!digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()))
                .then(|| parts[0].parse::<i64>().ok())
                .flatten();
            if atoi.is_none_or(|i| i < 0) {
                return Err("RJP must begin with positive integer".to_string());
            }
            let rest = &s[parts[0].len()..];
            if rest.starts_with('#') {
                Ok(())
            } else {
                check_json_pointer(rest)
            }
        }
        "regex" => crate::goregex::compile(s)
            .map(|_| ())
            .map_err(|_| "invalid regex expression".to_string()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn template() -> Schema {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/config-templates/standard/config/schema.json"
        ))
        .unwrap();
        Schema::compile(&bytes).unwrap()
    }

    fn msgs(errs: &[KeyError]) -> Vec<(String, String, String)> {
        errs.iter()
            .map(|e| {
                (
                    e.property_path.clone(),
                    e.message.clone(),
                    crate::gojson::fmt_v(&e.invalid_value),
                )
            })
            .collect()
    }

    #[test]
    fn errors_sorted_by_path_like_go() {
        // Go sorts the errors by path (byte order, stable); "/lockResources/10" comes
        // before "/lockResources/2".
        let s = template();
        let mut resources: Vec<Value> = (0..11).map(|i| json!(format!("r{i}"))).collect();
        resources[2] = json!(2);
        resources[10] = json!(10);
        let errs = s.validate(&json!({
            "topic": ["a"], "priority": 1.5, "properties": [1, 2],
            "lockResources": resources, "delay": "x"
        }));
        let paths: Vec<&str> = errs.iter().map(|e| e.property_path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/delay",
                "/lockResources/10",
                "/lockResources/2",
                "/priority",
                "/properties",
                "/topic"
            ]
        );
    }
    #[test]
    fn template_schema() {
        let s = template();
        assert!(s.validate(&json!({"properties": {"a": 1}})).is_empty());
        let example = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/config-templates/standard/config/job_example01.json"
        ))
        .unwrap();
        assert!(
            s.validate(&serde_json::from_slice(&example).unwrap())
                .is_empty()
        );
        assert_eq!(
            msgs(&s.validate(&json!({"topic": 5}))),
            vec![
                (
                    "/".into(),
                    "\"properties\" value is required".into(),
                    "map[topic:5]".into()
                ),
                (
                    "/topic".into(),
                    "type should be string, got integer".into(),
                    "5".into()
                )
            ]
        );
        assert_eq!(
            msgs(&s.validate(&json!({"properties": {}, "bogus": "x"}))),
            vec![(
                "/".into(),
                "additional properties are not allowed".into(),
                "map[bogus:x properties:map[]]".into()
            )]
        );
        // The Go template omitted callbackUrl, so jobs with a callback were refused.
        assert!(
            s.validate(&json!({"properties": {}, "callbackUrl": "https://example.com/cb"}))
                .is_empty()
        );
        assert_eq!(
            msgs(
                &s.validate(&json!({"properties": {}, "priority": 1.5, "lockResources": ["a", 2]}))
            ),
            vec![
                (
                    "/lockResources/1".into(),
                    "type should be string, got integer".into(),
                    "2".into()
                ),
                (
                    "/priority".into(),
                    "type should be integer, got number".into(),
                    "1.5".into()
                ),
            ]
        );
        assert_eq!(
            msgs(&s.validate(&json!("x"))),
            vec![(
                "/".into(),
                "type should be object, got string".into(),
                "x".into()
            )]
        );
    }

    #[test]
    fn keywords() {
        let s = Schema::compile(
            br#"{"type": ["string", "null"], "enum": ["a", 1, null], "maxLength": 1}"#,
        )
        .unwrap();
        assert!(s.validate(&json!("a")).is_empty());
        assert_eq!(
            msgs(&s.validate(&json!("ab")))[0].1,
            "should be one of [\"a\", 1, null]"
        );
        assert_eq!(
            msgs(&s.validate(&json!(true)))[0].1,
            "type should be one of: string,null, got boolean"
        );
        let s = Schema::compile(br#"{"type": "boolean"}"#).unwrap();
        assert!(s.validate(&json!("true")).is_empty());
        assert!(!s.validate(&json!("yes")).is_empty());
        let s =
            Schema::compile(br#"{"minimum": 2, "exclusiveMaximum": 10, "multipleOf": 2}"#).unwrap();
        assert_eq!(
            msgs(&s.validate(&json!(11)))
                .iter()
                .map(|m| m.1.clone())
                .collect::<Vec<_>>(),
            ["must be a multiple of 2", "11 must be less than 10"]
        );
        let s = Schema::compile(br##"{"$defs": {"n": {"type": "integer"}}, "properties": {"a": {"$ref": "#/$defs/n"}, "b": {"$ref": "#/definitions/m"}}, "definitions": {"m": {"type": "string"}}}"##).unwrap();
        assert!(s.validate(&json!({"a": 1, "b": "x"})).is_empty());
        assert_eq!(msgs(&s.validate(&json!({"a": "x"})))[0].0, "/a");
        let s = Schema::compile(br#"{"$ref": "https://example.com/s.json"}"#).unwrap();
        assert_eq!(
            msgs(&s.validate(&json!(1)))
                .iter()
                .map(|m| m.1.clone())
                .collect::<Vec<_>>(),
            [
                "failed to resolve schema for ref https://example.com/s.json",
                "schema is nil"
            ]
        );
        let s = Schema::compile(br##"{"properties": {"n": {"$ref": "#"}}, "required": ["v"]}"##)
            .unwrap();
        assert_eq!(
            s.validate(&json!({"v": 1, "n": {"v": 2, "n": {}}})).len(),
            1
        );
        let s = Schema::compile(
            br#"{"items": [{"type": "string"}], "additionalItems": false, "uniqueItems": true}"#,
        )
        .unwrap();
        assert_eq!(
            msgs(&s.validate(&json!(["a", "a"])))
                .iter()
                .map(|m| m.1.clone())
                .collect::<Vec<_>>(),
            [
                "array items must be unique. duplicated entry: a",
                "additional items are not allowed"
            ]
        );
        let s = Schema::compile(br#"{"if": {"required": ["a"]}, "then": {"required": ["b"]}, "else": {"required": ["c"]}}"#).unwrap();
        assert!(s.validate(&json!({"a": 1, "b": 1})).is_empty());
        assert_eq!(
            msgs(&s.validate(&json!({})))[0].1,
            "\"c\" value is required"
        );
        let s = Schema::compile(br#"{"const": {"a": [1, 2, 3, 4, 5, 6, 7]}}"#).unwrap();
        assert_eq!(
            msgs(&s.validate(&json!(1)))[0].1,
            "must equal {\"a\":[1,2,3,4,5,6,7]..."
        );
        let s = Schema::compile(br#"{"format": "date-time"}"#).unwrap();
        assert!(s.validate(&json!("2026-10-16T12:00:00.5+02:00")).is_empty());
        assert!(
            s.validate(&json!("2026-02-30T12:00:00Z"))[0]
                .message
                .starts_with("invalid date-time: date-time incorrectly Formatted")
        );
    }

    /// URI formats use the checks of Go's `url.Parse`; only `*-reference` and `uri-template`
    /// refuse backslashes.
    #[test]
    fn uri_formats_like_go() {
        let check = |format: &str, value: &str| {
            Schema::compile(format!(r#"{{"format": "{format}"}}"#).as_bytes())
                .unwrap()
                .validate(&json!(value))
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
        };
        assert!(check("uri", "https://example.com/a?b#c").is_empty());
        assert_eq!(
            check("uri", "2026-10-18T12:00:00Z"),
            [
                "invalid uri: uri incorrectly Formatted: parse \"2026-10-18T12:00:00Z\": first path segment in URL cannot contain colon"
            ]
        );
        assert_eq!(
            check("uri-reference", "::1"),
            [
                "invalid uri-reference: uri incorrectly Formatted: parse \"::1\": missing protocol scheme"
            ]
        );
        assert_eq!(
            check("iri", "http://a b/"),
            [
                "invalid iri: uri incorrectly Formatted: parse \"http://a b/\": invalid character \" \" in host name"
            ]
        );
        assert!(check("uri", "http://a/b\\c").is_empty());
        assert_eq!(
            check("iri-reference", "http://a/b\\c"),
            ["invalid iri-reference: invalid uri"]
        );
        assert_eq!(
            check("uri", "/a/b"),
            ["invalid uri: uri missing scheme prefix"]
        );
        assert_eq!(
            check("uri-template", "http://{host}:{port/"),
            ["invalid uri-template: invalid uri template"]
        );
        assert_eq!(
            check("uri-template", "1{x}:80"),
            [
                "invalid uri-template: uri incorrectly Formatted: parse \"1aaa:80\": first path segment in URL cannot contain colon"
            ]
        );
        assert!(check("uri-template", "{x}:80").is_empty());
    }

    /// A subschema reached through `$ref` keeps its text: `enum` lists the values as written.
    #[test]
    fn referenced_schema_keeps_its_text() {
        // JSON escapes built at run time (a backslash, `u` and four hex digits).
        let esc = |hex: &str| format!("\"{}u{hex}\"", '\\');
        let values = format!("[{}, 1e2, 1.0, {}]", esc("003c"), esc("00e9"));
        let schema =
            format!(r##"{{"$ref": "#/$defs/e", "$defs": {{"e": {{"enum": {values}}}}}}}"##);
        let s = Schema::compile(schema.as_bytes()).unwrap();
        assert_eq!(
            msgs(&s.validate(&json!("x")))[0].1,
            format!("should be one of {values}")
        );
        assert!(s.validate(&json!(100)).is_empty());
        assert!(s.validate(&json!("<")).is_empty());
    }

    fn format_errors(format: &str, value: &str) -> Vec<String> {
        Schema::compile(format!(r#"{{"format": "{format}"}}"#).as_bytes())
            .unwrap()
            .validate(&json!(value))
            .into_iter()
            .map(|e| e.message)
            .collect()
    }

    /// `date-time`, `date` and `time` use Go's `time.Parse`, which accepts more than
    /// RFC 3339 (one-digit hour, comma before the fraction, offset up to +24:60).
    #[test]
    fn date_time_formats_like_go() {
        for (f, v) in [
            ("date-time", "2026-10-18T12:00:00Z"),
            ("date-time", "2026-10-18t12:00:00.5+02:00"),
            ("date-time", "2026-10-18T9:00:00Z"),
            ("date-time", "2024-02-29T12:00:00,123456789012Z"),
            ("date-time", "2026-10-18T12:00:00+24:60"),
            ("time", "9:01:02Z"),
            ("date", "2026-10-18"),
        ] {
            assert!(format_errors(f, v).is_empty(), "{f} {v}");
        }
        let prefix = "invalid date-time: date-time incorrectly Formatted: parsing time";
        for (f, v, e) in [
            (
                "date-time",
                "2026-10-18",
                r#""2026-10-18" as "2006-01-02T15:04:05Z07:00": cannot parse "" as "T""#,
            ),
            (
                "date-time",
                "2026-1-18T12:00:00Z",
                r#""2026-1-18T12:00:00Z" as "2006-01-02T15:04:05Z07:00": cannot parse "1-18T12:00:00Z" as "01""#,
            ),
            (
                "date-time",
                "2026-13-18T12:00:00Z",
                r#""2026-13-18T12:00:00Z": month out of range"#,
            ),
            (
                "date-time",
                "2026-02-29T12:00:00Z",
                r#""2026-02-29T12:00:00Z": day out of range"#,
            ),
            (
                "date-time",
                "2026-10-18T24:00:00Z",
                r#""2026-10-18T24:00:00Z": hour out of range"#,
            ),
            (
                "date-time",
                "2026-10-18T12:00:00+25:00",
                r#""2026-10-18T12:00:00+25:00": time zone offset hour out of range"#,
            ),
            (
                "date-time",
                "2026-10-18T12:00:00Zx",
                r#""2026-10-18T12:00:00ZX": extra text: "X""#,
            ),
            (
                "date-time",
                "é",
                r#""\xc3\x89" as "2006-01-02T15:04:05Z07:00": cannot parse "\xc3\x89" as "2006""#,
            ),
            (
                "date-time",
                "2026-10-18T12:00:00+0200",
                r#""2026-10-18T12:00:00+0200" as "2006-01-02T15:04:05Z07:00": cannot parse "+0200" as "Z07:00""#,
            ),
        ] {
            assert_eq!(format_errors(f, v), [format!("{prefix} {e}")], "{v}");
        }
        assert_eq!(
            format_errors("date", "2026-10-32"),
            [
                r#"invalid date: date-time incorrectly Formatted: parsing time "2026-10-32T08:30:06.283185Z": day out of range"#
            ]
        );
    }

    /// `email` and `idn-email` use Go's `mail.ParseAddress`.
    #[test]
    fn email_formats_like_go() {
        for v in [
            "a@b.c",
            "A B <a@b.c>",
            "\"x y\"@b",
            "a@[127.0.0.1]",
            "g: a@b;",
        ] {
            assert!(format_errors("email", v).is_empty(), "{v}");
        }
        for (v, e) in [
            ("a", "mail: missing '@' or angle-addr"),
            ("a b", "mail: no angle-addr"),
            ("a@b c", "mail: expected single address, got \"c\""),
            (
                "<a@[1.2.3]>",
                "mail: invalid IP address in domain-literal: \"1.2.3\"",
            ),
            (
                "]]02@2000190000",
                "mail: missing word in phrase: mail: invalid string",
            ),
        ] {
            assert_eq!(
                format_errors("idn-email", v),
                [format!(
                    "invalid idn-email: email address incorrectly Formatted: {e}"
                )]
            );
        }
    }

    /// The number of a relative JSON pointer is read with `strconv.Atoi`.
    #[test]
    fn relative_json_pointer_like_go() {
        for v in ["0", "1/a", "+1", "-0/a", "2#", "9223372036854775807"] {
            assert!(format_errors("relative-json-pointer", v).is_empty(), "{v}");
        }
        for v in ["", "-1", "a", "1a", "9223372036854775808", "1/a~2", "+"] {
            assert!(!format_errors("relative-json-pointer", v).is_empty(), "{v}");
        }
    }

    /// Patterns follow Go's regexp syntax and its ASCII-only `\d`, `\w`, `\s`, `\b`.
    #[test]
    fn go_regexp_patterns() {
        let s = Schema::compile(
            br#"{"pattern": "^\\d+$", "patternProperties": {"^x{": {"format": "regex"}}}"#,
        )
        .unwrap();
        assert!(s.validate(&json!("123")).is_empty());
        assert_eq!(
            msgs(&s.validate(&json!("١٢٣"))),
            vec![(
                "/".into(),
                "regexp pattern ^\\d+$ mismatch on string: ١٢٣".into(),
                "١٢٣".into()
            )]
        );
        assert!(s.validate(&json!({"x{": "a{"})).is_empty());
        assert_eq!(
            msgs(&s.validate(&json!({"x{": "a**"}))),
            vec![(
                "/x{".into(),
                "invalid regex: invalid regex expression".into(),
                "a**".into()
            )]
        );
        assert_eq!(
            Schema::compile(br#"{"pattern": "\\e"}"#).unwrap_err(),
            "error unmarshaling pattern from json: error parsing regexp: invalid escape sequence: `\\e`"
        );
        assert_eq!(
            Schema::compile(br#"{"patternProperties": {"(": {}}}"#).unwrap_err(),
            "error unmarshaling patternProperties from json: invalid pattern: (: error parsing regexp: missing closing ): `(`"
        );
    }

    #[test]
    fn compile_errors() {
        assert!(Schema::compile(b"{").is_err());
        assert_eq!(
            Schema::compile(br#"{"type": "text"}"#).unwrap_err(),
            "error unmarshaling type from json: \"text\" is not a valid type"
        );
        assert!(Schema::compile(br#"{"maxLength": 1.5}"#).is_err());
        assert!(
            Schema::compile(b"true")
                .unwrap()
                .validate(&json!(1))
                .is_empty()
        );
        assert_eq!(
            Schema::compile(b"false").unwrap().validate(&json!(1))[0].message,
            "schema is always false"
        );
    }

    #[test]
    fn schema_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Schema>();
    }
}
