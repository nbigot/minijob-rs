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

/// Invalid UTF-8 is replaced by U+FFFD, as Go's decoder does, instead of being rejected.
fn lossy(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// Decodes a document like `json.Unmarshal(data, &v)` with `v interface{}`.
pub fn parse_value(bytes: &[u8]) -> Result<Value, DecodeError> {
    serde_json::from_str(&lossy(bytes)).map_err(|e| DecodeError(e.to_string()))
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
    let text = lossy(bytes);
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
