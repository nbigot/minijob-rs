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
    Pattern(Regex),
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

fn parse_raw<'a, T: serde::Deserialize<'a>>(prop: &str, raw: &'a RawValue) -> Result<T, String> {
    serde_json::from_str(raw.get()).map_err(|e| json_err(prop, e))
}

fn go_int(prop: &str, raw: &RawValue, go_type: &str) -> Result<i64, String> {
    let v: Value = parse_raw(prop, raw)?;
    match v.as_i64() {
        Some(i) => Ok(i),
        None => Err(json_err(
            prop,
            format!(
                "json: cannot unmarshal {} into Go value of type jsonschema.{go_type}",
                kind_of(&v)
            ),
        )),
    }
}

fn kind_of(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(_) => "bool".into(),
        Value::Number(n) => format!("number {n}"),
        Value::String(_) => "string".into(),
        Value::Array(_) => "array".into(),
        Value::Object(_) => "object".into(),
    }
}

/// Evaluates a JSON pointer (RFC 6901) over raw JSON text.
fn eval_pointer(text: &str, pointer: &str) -> Option<String> {
    let mut current: Value = serde_json::from_str(text).ok()?;
    if pointer.is_empty() {
        return Some(text.to_string());
    }
    for token in pointer.strip_prefix('/')?.split('/') {
        let token = percent_encoding::percent_decode_str(token)
            .decode_utf8_lossy()
            .replace("~1", "/")
            .replace("~0", "~");
        current = match current {
            Value::Object(mut m) => m.remove(&token)?,
            Value::Array(mut a) => {
                let i: usize = token.parse().ok()?;
                if i >= a.len() {
                    return None;
                }
                a.swap_remove(i)
            }
            _ => return None,
        };
    }
    serde_json::to_string(&current).ok()
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

    fn compile(&mut self, prop: &str, raw: &RawValue) -> Result<NodeId, String> {
        let text = raw.get().trim();
        if text == "true" {
            return Ok(self.push(Node::True));
        }
        if text == "false" {
            return Ok(self.push(Node::False));
        }
        let members: BTreeMap<String, &RawValue> =
            serde_json::from_str(raw.get()).map_err(|e| {
                if prop.is_empty() {
                    e.to_string()
                } else {
                    json_err(prop, e)
                }
            })?;
        let id = self.push(Node::Pending);
        let mut named: Vec<(&str, Kw)> = Vec::new();
        for (name, raw) in &members {
            if let Some(kw) = self.keyword(name, raw)? {
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

    fn schema_map(
        &mut self,
        prop: &str,
        raw: &RawValue,
    ) -> Result<BTreeMap<String, NodeId>, String> {
        let m: BTreeMap<String, &RawValue> = parse_raw(prop, raw)?;
        let mut out = BTreeMap::new();
        for (k, v) in m {
            out.insert(k, self.compile(prop, v)?);
        }
        Ok(out)
    }

    fn schema_list(&mut self, prop: &str, raw: &RawValue) -> Result<Vec<NodeId>, String> {
        let l: Vec<&RawValue> = parse_raw(prop, raw)?;
        l.into_iter().map(|v| self.compile(prop, v)).collect()
    }

    fn regex(prop: &str, pattern: &str) -> Result<Regex, String> {
        Regex::new(pattern).map_err(|e| json_err(prop, format!("invalid pattern: {pattern}: {e}")))
    }

    fn keyword(&mut self, name: &str, raw: &RawValue) -> Result<Option<Kw>, String> {
        let p = name;
        Ok(Some(match name {
            "$ref" | "$recursiveRef" => {
                let r: String = if name == "$ref" {
                    parse_raw(p, raw)?
                } else {
                    "#".into()
                };
                let target = self.resolve(&r)?;
                Kw::Ref(r, target)
            }
            "type" => {
                let v: Value = parse_raw(p, raw)?;
                let vals: Vec<String> = match v {
                    Value::String(s) => vec![s],
                    Value::Array(a) => a
                        .into_iter()
                        .map(|x| {
                            x.as_str()
                                .map(str::to_string)
                                .ok_or_else(|| json_err(p, "json: cannot unmarshal into []string"))
                        })
                        .collect::<Result<_, _>>()?,
                    _ => return Err(json_err(p, "json: cannot unmarshal into []string")),
                };
                for t in &vals {
                    if ![
                        "null", "boolean", "object", "array", "number", "string", "integer",
                    ]
                    .contains(&t.as_str())
                    {
                        return Err(json_err(p, format!("\"{t}\" is not a valid type")));
                    }
                }
                Kw::Type(vals)
            }
            "enum" => {
                let items: Vec<&RawValue> = parse_raw(p, raw)?;
                Kw::Enum(
                    items
                        .into_iter()
                        .map(|r| {
                            (
                                serde_json::from_str(r.get()).unwrap_or(Value::Null),
                                r.get().to_string(),
                            )
                        })
                        .collect(),
                )
            }
            "const" => Kw::Const(parse_raw(p, raw)?),
            "multipleOf" => Kw::MultipleOf(parse_raw(p, raw)?),
            "maximum" => Kw::Maximum(parse_raw(p, raw)?),
            "exclusiveMaximum" => Kw::ExclusiveMaximum(parse_raw(p, raw)?),
            "minimum" => Kw::Minimum(parse_raw(p, raw)?),
            "exclusiveMinimum" => Kw::ExclusiveMinimum(parse_raw(p, raw)?),
            "maxLength" => Kw::MaxLength(go_int(p, raw, "MaxLength")?),
            "minLength" => Kw::MinLength(go_int(p, raw, "MinLength")?),
            "pattern" => {
                let s: String = parse_raw(p, raw)?;
                Kw::Pattern(Self::regex(p, &s)?)
            }
            "allOf" => Kw::AllOf(self.schema_list(p, raw)?),
            "anyOf" => Kw::AnyOf(self.schema_list(p, raw)?),
            "oneOf" => Kw::OneOf(self.schema_list(p, raw)?),
            "not" => Kw::Not(self.compile(p, raw)?),
            "properties" => Kw::Properties(self.schema_map(p, raw)?),
            "patternProperties" => {
                let m = self.schema_map(p, raw)?;
                let mut list = Vec::new();
                for (k, v) in m {
                    list.push((Self::regex(p, &k)?, v));
                }
                Kw::PatternProperties(list)
            }
            "additionalProperties" => Kw::AdditionalProperties(self.compile(p, raw)?),
            "required" => Kw::Required(parse_raw(p, raw)?),
            "propertyNames" => Kw::PropertyNames(self.compile(p, raw)?),
            "maxProperties" => Kw::MaxProperties(go_int(p, raw, "MaxProperties")?),
            "minProperties" => Kw::MinProperties(go_int(p, raw, "MinProperties")?),
            "dependentSchemas" => Kw::DependentSchemas(self.schema_map(p, raw)?),
            "dependentRequired" => Kw::DependentRequired(parse_raw(p, raw)?),
            "unevaluatedProperties" => Kw::UnevaluatedProperties(self.compile(p, raw)?),
            "items" => {
                if raw.get().trim_start().starts_with('[') {
                    Kw::Items {
                        single: false,
                        schemas: self.schema_list(p, raw)?,
                    }
                } else {
                    Kw::Items {
                        single: true,
                        schemas: vec![self.compile(p, raw)?],
                    }
                }
            }
            "additionalItems" => Kw::AdditionalItems(self.compile(p, raw)?),
            "maxItems" => Kw::MaxItems(go_int(p, raw, "MaxItems")?),
            "minItems" => Kw::MinItems(go_int(p, raw, "MinItems")?),
            "uniqueItems" => {
                let _: bool = parse_raw(p, raw)?;
                Kw::UniqueItems
            }
            "contains" => Kw::Contains(self.compile(p, raw)?, raw.get().to_string()),
            "maxContains" => Kw::MaxContains(go_int(p, raw, "MaxContains")?),
            "minContains" => Kw::MinContains(go_int(p, raw, "MinContains")?),
            "unevaluatedItems" => Kw::UnevaluatedItems(self.compile(p, raw)?),
            "if" => Kw::If(self.compile(p, raw)?),
            "then" => Kw::Then(self.compile(p, raw)?),
            "else" => Kw::Else(self.compile(p, raw)?),
            "format" => Kw::Format(parse_raw(p, raw)?),
            "$defs" => {
                // Validated for structure only; reached through `$ref`.
                let _: BTreeMap<String, &RawValue> = parse_raw(p, raw)?;
                return Ok(None);
            }
            "$id" | "$anchor" | "$schema" | "$comment" | "title" | "description" => {
                let _: Value = parse_raw(p, raw)?;
                return Ok(None);
            }
            // Annotations, unsupported keywords (`definitions`, `dependencies`, ...) and
            // unknown members are ignored.
            _ => return Ok(None),
        }))
    }

    /// Resolves a reference inside the document; `None` when it points elsewhere.
    fn resolve(&mut self, reference: &str) -> Result<Option<NodeId>, String> {
        let Some(fragment) = reference.strip_prefix('#') else {
            return Ok(None);
        };
        let pointer = if fragment.is_empty() || fragment.starts_with('/') {
            fragment.to_string()
        } else {
            let root: Value = serde_json::from_str(self.root_text).unwrap_or(Value::Null);
            match find_anchor(&root, fragment, &mut Vec::new()) {
                Some(p) => p,
                None => return Ok(None),
            }
        };
        if let Some(id) = self.by_pointer.get(&pointer) {
            return Ok(Some(*id));
        }
        let Some(text) = eval_pointer(self.root_text, &pointer) else {
            return Ok(None);
        };
        if pointer.is_empty() {
            // The root itself: its id is 0 once compiled.
            return Ok(Some(0));
        }
        let raw: Box<RawValue> = RawValue::from_string(text).map_err(|e| e.to_string())?;
        // Reserve the id before compiling to support recursive references.
        let reserved = self.nodes.len();
        self.by_pointer.insert(pointer.clone(), reserved);
        let id = self.compile("$ref", &raw)?;
        if id != reserved {
            self.by_pointer.insert(pointer, id);
        }
        Ok(Some(id))
    }
}

impl Schema {
    /// Compiles a schema document (`Schema.UnmarshalJSON`).
    pub fn compile(bytes: &[u8]) -> Result<Schema, String> {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let raw: &RawValue = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let mut c = Compiler {
            root_text: raw.get(),
            nodes: Vec::new(),
            by_pointer: HashMap::new(),
        };
        let root = c.compile("", raw)?;
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
            Kw::Pattern(re) => {
                if let Some(s) = data.as_str()
                    && !re.is_match(s)
                {
                    st.add_error(
                        data,
                        format!("regexp pattern {} mismatch on string: {s}", re.as_str()),
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

/// Go `time.Parse(time.RFC3339, s)` acceptance (fractional seconds allowed).
fn is_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| {
        b.get(r.clone())
            .is_some_and(|x| x.iter().all(u8::is_ascii_digit))
    };
    let num =
        |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<u32>().ok()).unwrap_or(99);
    if b.len() < 20
        || !digits(0..4)
        || b[4] != b'-'
        || !digits(5..7)
        || b[7] != b'-'
        || !digits(8..10)
        || b[10] != b'T'
        || !digits(11..13)
        || b[13] != b':'
        || !digits(14..16)
        || b[16] != b':'
        || !digits(17..19)
    {
        return false;
    }
    let (month, day, hour, min, sec) =
        (num(5..7), num(8..10), num(11..13), num(14..16), num(17..19));
    let year: i32 = s[0..4].parse().unwrap_or(0);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    if day == 0 || day > days || hour > 23 || min > 59 || sec > 59 {
        return false;
    }
    let mut i = 19;
    if b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match &s[i..] {
        "Z" => true,
        z if z.len() == 6
            && (z.starts_with('+') || z.starts_with('-'))
            && z.as_bytes()[3] == b':'
            && z[1..3].bytes().all(|c| c.is_ascii_digit())
            && z[4..6].bytes().all(|c| c.is_ascii_digit()) =>
        {
            num(i + 1..i + 3) < 24 && num(i + 4..i + 6) < 60
        }
        _ => false,
    }
}

fn check_uri_ref(s: &str) -> Result<(), String> {
    if s.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
        return Err(format!(
            "uri incorrectly Formatted: parse {s:?}: net/url: invalid control character in URL"
        ));
    }
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let ok = b.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
            if !ok {
                let esc: String = s[i..].chars().take(3).collect();
                return Err(format!(
                    "uri incorrectly Formatted: parse {s:?}: invalid URL escape {esc:?}"
                ));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
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
        if is_rfc3339(&dt.to_uppercase()) {
            Ok(())
        } else {
            Err(format!(
                "date-time incorrectly Formatted: parsing time {:?} as \"2006-01-02T15:04:05Z07:00\": cannot parse {:?} as \"2006\"",
                dt.to_uppercase(),
                dt.to_uppercase()
            ))
        }
    };
    match format {
        "date-time" => datetime(s),
        "date" => datetime(&format!("{s}T08:30:06.283185Z")),
        "time" => datetime(&format!("1963-06-19T{s}")),
        "email" | "idn-email" => {
            let addr = s
                .rsplit_once('<')
                .map_or(s, |(_, a)| a.trim_end_matches('>'));
            match addr.split_once('@') {
                Some((l, d)) if !l.is_empty() && !d.is_empty() && !d.contains('@') => Ok(()),
                _ => Err(
                    "email address incorrectly Formatted: mail: missing '@' or angle-addr"
                        .to_string(),
                ),
            }
        }
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
            check_uri_ref(s)?;
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
            if parts[0].is_empty() || !parts[0].bytes().all(|c| c.is_ascii_digit()) {
                return Err("RJP must begin with positive integer".to_string());
            }
            let rest = &s[parts[0].len()..];
            if rest.starts_with('#') {
                Ok(())
            } else {
                check_json_pointer(rest)
            }
        }
        "regex" => Regex::new(s)
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
            msgs(&s.validate(&json!({"properties": {}, "callbackUrl": "x"}))),
            vec![(
                "/".into(),
                "additional properties are not allowed".into(),
                "map[callbackUrl:x properties:map[]]".into()
            )]
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
