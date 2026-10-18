//! Decoding with the semantics of Go's `json.Unmarshal` into structs.

use serde::Deserializer;
use serde::de::{self, MapAccess, Visitor};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::fmt;

/// Object members in document order, duplicates kept (Go applies them in order).
pub type Entries = Vec<(String, Value)>;

/// A decoding failure. Go never exposes the message in API responses, so it is only logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// The text of a JSON document as Go's decoder reads it: invalid UTF-8 and `\u` escapes of
/// unpaired UTF-16 surrogates become U+FFFD instead of being rejected (serde_json refuses
/// lone surrogates).
pub fn go_text(bytes: &[u8]) -> Cow<'_, str> {
    match String::from_utf8_lossy(bytes) {
        Cow::Borrowed(s) => replace_lone_surrogates(s).map_or(Cow::Borrowed(s), Cow::Owned),
        Cow::Owned(s) => Cow::Owned(replace_lone_surrogates(&s).unwrap_or(s)),
    }
}

fn hex4(b: &[u8]) -> Option<u16> {
    let s = std::str::from_utf8(b.get(..4)?).ok()?;
    u16::from_str_radix(s, 16).ok()
}

/// Rewrites `\uXXXX` escapes of unpaired surrogates as `�` (Go `unquote`: a high
/// surrogate followed by a `\u` low surrogate is a pair, any other surrogate is U+FFFD).
/// Returns `None` when there is nothing to change.
fn replace_lone_surrogates(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let is_high = |u: u16| (0xD800..0xDC00).contains(&u);
    let is_low = |u: u16| (0xDC00..0xE000).contains(&u);
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut in_string = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => in_string = !in_string,
            b'\\' if in_string => {
                if b.get(i + 1) == Some(&b'u')
                    && let Some(u) = hex4(&b[i + 2..])
                    && (is_high(u) || is_low(u))
                {
                    let paired = is_high(u)
                        && b.get(i + 6) == Some(&b'\\')
                        && b.get(i + 7) == Some(&b'u')
                        && hex4(&b[(i + 8).min(b.len())..]).is_some_and(is_low);
                    if paired {
                        i += 12;
                    } else {
                        let o = out.get_or_insert_with(|| String::with_capacity(text.len()));
                        o.push_str(&text[copied..i]);
                        o.push_str("\\ufffd");
                        i += 6;
                        copied = i;
                    }
                    continue;
                }
                // Any other escape: skip the escaped character (it may be a quote).
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.map(|mut o| {
        o.push_str(&text[copied..]);
        o
    })
}

/// Decodes a document like `json.Unmarshal(data, &v)` with `v interface{}`.
pub fn parse_value(bytes: &[u8]) -> Result<Value, DecodeError> {
    serde_json::from_str(&go_text(bytes)).map_err(|e| DecodeError(e.to_string()))
}

/// A top-level document decoded for a Go struct target.
#[derive(Debug, PartialEq)]
pub enum Top {
    /// `null`: Go leaves the struct untouched.
    Null,
    /// An object, members in document order.
    Object(Entries),
}

enum TopOrOther {
    Top(Top),
    Other(&'static str),
}

struct TopVisitor;

impl<'de> Visitor<'de> for TopVisitor {
    type Value = TopOrOther;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object or null")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(TopOrOther::Top(Top::Null))
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(TopOrOther::Top(Top::Null))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(TopOrOther::Other("bool"))
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(TopOrOther::Other("number"))
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(TopOrOther::Other("number"))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(TopOrOther::Other("number"))
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(TopOrOther::Other("string"))
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<de::IgnoredAny>()?.is_some() {}
        Ok(TopOrOther::Other("array"))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            let value: Value = map.next_value()?;
            entries.push((key, value));
        }
        Ok(TopOrOther::Top(Top::Object(entries)))
    }
}

/// Decodes a document that Go decodes into a struct of type `go_type`.
pub fn parse_top(bytes: &[u8], go_type: &str) -> Result<Top, DecodeError> {
    let text = go_text(bytes);
    let mut de = serde_json::Deserializer::from_str(&text);
    let parsed = (&mut de)
        .deserialize_any(TopVisitor)
        .map_err(|e| DecodeError(e.to_string()))?;
    de.end().map_err(|e| DecodeError(e.to_string()))?;
    match parsed {
        TopOrOther::Top(top) => Ok(top),
        TopOrOther::Other(kind) => Err(DecodeError(format!(
            "json: cannot unmarshal {kind} into Go value of type {go_type}"
        ))),
    }
}

/// Case folding used by Go to match JSON keys with struct field names (`bytes.EqualFold`).
///
/// Field names are ASCII, so a key matches when each of its characters folds to the
/// corresponding ASCII letter; besides ASCII case, Unicode folds U+017F (ſ) to `s` and U+212A
/// (Kelvin sign) to `k`.
fn fold_eq_ascii(key: &str, field: &str) -> bool {
    let mut f = field.bytes();
    for c in key.chars() {
        let folded = match c {
            '\u{17F}' => b's',
            '\u{212A}' => b'k',
            c if c.is_ascii() => (c as u8).to_ascii_lowercase(),
            _ => return false,
        };
        match f.next() {
            Some(b) if b.to_ascii_lowercase() == folded => {}
            _ => return false,
        }
    }
    f.next().is_none()
}

/// Index of the struct field a JSON key maps to: exact name first, then case-insensitive.
pub fn match_field(key: &str, fields: &[&str]) -> Option<usize> {
    fields
        .iter()
        .position(|f| *f == key)
        .or_else(|| fields.iter().position(|f| fold_eq_ascii(key, f)))
}

/// Type mismatch while decoding a member (Go's `UnmarshalTypeError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeMismatch;

/// `null` leaves a string unchanged; any other non-string is an error.
pub fn set_string(v: &Value, out: &mut String) -> Result<(), TypeMismatch> {
    match v {
        Value::Null => Ok(()),
        Value::String(s) => {
            out.clone_from(s);
            Ok(())
        }
        _ => Err(TypeMismatch),
    }
}

/// Integer value of a JSON number literal as Go's `strconv.ParseInt` accepts it.
fn as_go_i64(v: &Value) -> Result<Option<i64>, TypeMismatch> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Some(i))
            } else if let Some(f) = n.as_f64().filter(|_| !n.is_u64()) {
                // serde_json turns the literal `-0` into a float; Go reads it as 0.
                if f == 0.0 && f.is_sign_negative() {
                    Ok(Some(0))
                } else {
                    Err(TypeMismatch)
                }
            } else {
                Err(TypeMismatch)
            }
        }
        _ => Err(TypeMismatch),
    }
}

/// Signed integer with Go's overflow check for the target size.
pub fn set_int<T: TryFrom<i64>>(v: &Value, out: &mut T) -> Result<(), TypeMismatch> {
    if let Some(i) = as_go_i64(v)? {
        *out = T::try_from(i).map_err(|_| TypeMismatch)?;
    }
    Ok(())
}

/// Unsigned integer (Go `uint`, 64 bits).
pub fn set_uint(v: &Value, out: &mut u64) -> Result<(), TypeMismatch> {
    match v {
        Value::Null => Ok(()),
        Value::Number(n) => match n.as_u64() {
            Some(u) => {
                *out = u;
                Ok(())
            }
            None => match n.as_f64() {
                Some(f) if f == 0.0 && f.is_sign_negative() => {
                    *out = 0;
                    Ok(())
                }
                _ => Err(TypeMismatch),
            },
        },
        _ => Err(TypeMismatch),
    }
}

/// `[]string`: `null` sets nil, elements that are `null` become `""`.
pub fn set_string_slice(v: &Value, out: &mut Option<Vec<String>>) -> Result<(), TypeMismatch> {
    match v {
        Value::Null => {
            *out = None;
            Ok(())
        }
        Value::Array(items) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                let mut s = String::new();
                set_string(item, &mut s)?;
                list.push(s);
            }
            *out = Some(list);
            Ok(())
        }
        _ => Err(TypeMismatch),
    }
}

/// `map[string]interface{}`: `null` sets nil; an object is merged into an existing map, as Go
/// reuses a non-nil map.
pub fn set_map(v: &Value, out: &mut Option<Map<String, Value>>) -> Result<(), TypeMismatch> {
    match v {
        Value::Null => {
            *out = None;
            Ok(())
        }
        Value::Object(obj) => {
            match out {
                Some(existing) => {
                    for (k, val) in obj {
                        existing.insert(k.clone(), val.clone());
                    }
                }
                None => *out = Some(obj.clone()),
            }
            Ok(())
        }
        _ => Err(TypeMismatch),
    }
}

/// Members of a nested object value (sorted, duplicates already collapsed by serde_json).
pub fn entries_of(v: &Value) -> Result<Option<Entries>, TypeMismatch> {
    match v {
        Value::Null => Ok(None),
        Value::Object(obj) => Ok(Some(
            obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        )),
        _ => Err(TypeMismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn top_level() {
        assert_eq!(parse_top(b" null ", "T").unwrap(), Top::Null);
        assert!(parse_top(b"[1]", "T").is_err());
        assert!(parse_top(b"{} x", "T").is_err());
        assert!(parse_top(b"", "T").is_err());
        let Top::Object(e) = parse_top(br#"{"a":1,"A":2,"a":3}"#, "T").unwrap() else {
            panic!()
        };
        assert_eq!(e.len(), 3);
        assert_eq!(e[2], ("a".to_string(), json!(3)));
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let Top::Object(e) = parse_top(b"{\"topic\":\"a\xffb\"}", "T").unwrap() else {
            panic!()
        };
        assert_eq!(e[0].1, json!("a\u{FFFD}b"));
    }

    #[test]
    fn field_matching() {
        let fields = ["topic", "sessionId", "lockResources"];
        assert_eq!(match_field("topic", &fields), Some(0));
        assert_eq!(match_field("TOPIC", &fields), Some(0));
        assert_eq!(match_field("SESSIONID", &fields), Some(1));
        assert_eq!(match_field("loc\u{212A}Resources", &fields), Some(2));
        assert_eq!(match_field("topics", &fields), None);
        assert_eq!(match_field("topi", &fields), None);
    }

    #[test]
    fn integers_like_go() {
        let mut p: i8 = 5;
        set_int(&json!(null), &mut p).unwrap();
        assert_eq!(p, 5);
        set_int(&json!(-128), &mut p).unwrap();
        assert_eq!(p, -128);
        assert!(set_int(&json!(200), &mut p).is_err());
        assert!(set_int(&json!(1.5), &mut p).is_err());
        assert!(set_int(&json!("1"), &mut p).is_err());
        let v: Value = serde_json::from_str("-0").unwrap();
        set_int(&v, &mut p).unwrap();
        assert_eq!(p, 0);
        let mut u = 0u64;
        assert!(set_uint(&json!(-1), &mut u).is_err());
        set_uint(&json!(u64::MAX), &mut u).unwrap();
        assert_eq!(u, u64::MAX);
    }

    #[test]
    fn lone_surrogates_become_replacement_characters() {
        let s = |text: &str| match parse_value(text.as_bytes()).unwrap() {
            Value::String(s) => s,
            v => panic!("{v}"),
        };
        assert_eq!(s(r#""\ud800""#), "\u{FFFD}");
        assert_eq!(s(r#""a\udc00b""#), "a\u{FFFD}b");
        // `\u` escapes built at run time: "\\u{x}" is a backslash, `u` and the hex digits.
        let esc = |hex: &[&str]| hex.iter().map(|x| format!("\\u{x}")).collect::<String>();
        let pair = esc(&["d83d", "de00"]);
        assert_eq!(s(&format!("\"{pair}\"")), "\u{1F600}", "a pair is kept");
        let not_low = esc(&["d83d", "0041"]);
        assert_eq!(s(&format!("\"{not_low}\"")), "\u{FFFD}A");
        let high_then_pair = esc(&["d83d", "d83d", "de00"]);
        assert_eq!(s(&format!("\"{high_then_pair}\"")), "\u{FFFD}\u{1F600}");
        let valid = format!(r#"{{"a":"{}{pair}"}}"#, esc(&["00e9"]));
        assert!(replace_lone_surrogates(&valid).is_none());
        assert_eq!(s(r#""\ud83d""#), "\u{FFFD}", "at the end of the string");
        assert_eq!(
            s(r#""\\ud800""#),
            "\\ud800",
            "an escaped backslash is not an escape"
        );
        assert_eq!(s(r#""\"\ud800""#), "\"\u{FFFD}");
        // In keys and nested values too.
        let v = parse_value(br#"{"\udfff":["\ud800"]}"#).unwrap();
        assert_eq!(v, json!({"\u{FFFD}": ["\u{FFFD}"]}));
        // Outside strings, nothing is rewritten (the document stays invalid).
        assert!(parse_value(br#"\ud800"#).is_err());
        assert!(parse_top(br#"{"topic":"\udc00x"}"#, "job.JobRequest").is_ok());
    }

    #[test]
    fn maps_merge_and_slices() {
        let mut m = None;
        set_map(&json!({"a": 1}), &mut m).unwrap();
        set_map(&json!({"b": 2}), &mut m).unwrap();
        assert_eq!(Value::Object(m.clone().unwrap()), json!({"a": 1, "b": 2}));
        set_map(&json!(null), &mut m).unwrap();
        assert!(m.is_none());
        let mut s = None;
        set_string_slice(&json!(["x", null]), &mut s).unwrap();
        assert_eq!(s, Some(vec!["x".to_string(), String::new()]));
        assert!(set_string_slice(&json!([1]), &mut s).is_err());
    }
}
