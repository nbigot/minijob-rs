use serde::Serialize;
use serde_json::ser::Formatter;
use std::io;

/// `serde_json` formatter producing the same bytes as Go's `json.Marshal`.
///
/// Differences with the default `serde_json` formatter:
/// - `<`, `>`, `&`, U+2028 and U+2029 are escaped (`<`, ...), like Go's HTML-safe encoder;
/// - floats use Go's format: shortest digits, exponent only below 1e-6 or from 1e21, `1.0` → `1`.
///
/// Control characters, `"` and `\` are escaped by `serde_json` the same way Go does
/// (`\b \f \n \r \t`, otherwise `\u00XX` with lowercase hex).
#[derive(Clone, Copy, Debug, Default)]
pub struct GoFormatter;

impl Formatter for GoFormatter {
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        write_html_safe(writer, fragment)
    }

    fn write_f64<W>(&mut self, writer: &mut W, value: f64) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        let mut s = String::with_capacity(24);
        format_float(value, &mut s);
        writer.write_all(s.as_bytes())
    }

    fn write_f32<W>(&mut self, writer: &mut W, value: f32) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.write_f64(writer, f64::from(value))
    }
}

fn write_html_safe<W: ?Sized + io::Write>(writer: &mut W, fragment: &str) -> io::Result<()> {
    let bytes = fragment.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let escape: Option<&[u8]> = match bytes[i] {
            b'<' => Some(b"\\u003c"),
            b'>' => Some(b"\\u003e"),
            b'&' => Some(b"\\u0026"),
            // U+2028 and U+2029 are encoded as E2 80 A8 / E2 80 A9.
            0xE2 if i + 2 < bytes.len() && bytes[i + 1] == 0x80 => match bytes[i + 2] {
                0xA8 => Some(b"\\u2028"),
                0xA9 => Some(b"\\u2029"),
                _ => None,
            },
            _ => None,
        };
        match escape {
            Some(e) => {
                writer.write_all(&bytes[start..i])?;
                writer.write_all(e)?;
                i += if bytes[i] == 0xE2 { 3 } else { 1 };
                start = i;
            }
            None => i += 1,
        }
    }
    writer.write_all(&bytes[start..])
}

/// Serializes `value` like Go's `json.Marshal`.
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, GoFormatter);
    // Serialization into a Vec cannot fail for the types used in this program (string keys only).
    value
        .serialize(&mut ser)
        .expect("JSON serialization into memory");
    out
}

/// Appends `s` as a JSON string literal with Go's escaping.
pub fn write_string(out: &mut String, s: &str) {
    let mut buf = Vec::with_capacity(s.len() + 2);
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, GoFormatter);
    s.serialize(&mut ser).expect("JSON string serialization");
    // The serializer only emits valid UTF-8.
    out.push_str(std::str::from_utf8(&buf).unwrap_or_default());
}

/// Shortest decimal digits and decimal exponent of a finite, non-zero float.
///
/// Returns `(negative, digits, exp)` where the value is `0.d1d2d3... × 10^(exp + 1)`, i.e.
/// `exp` is the exponent of the first digit (`1.5e3` → digits `15`, exp `3`).
pub(crate) fn shortest_digits(f: f64) -> (bool, String, i32) {
    // Rust's `{:e}` prints the shortest representation that round-trips, like Go's
    // strconv with precision -1.
    let s = format!("{f:e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa
        .trim_start_matches('-')
        .chars()
        .filter(|c| *c != '.')
        .collect();
    (negative, digits, exp)
}

/// Writes `digits × 10^(exp+1-len)` in plain decimal notation (Go's `'f'` format with
/// precision -1).
pub(crate) fn push_plain(out: &mut String, digits: &str, exp: i32) {
    let dp = exp + 1; // digits before the decimal point
    let len = digits.len() as i32;
    if dp <= 0 {
        out.push_str("0.");
        for _ in 0..(-dp) {
            out.push('0');
        }
        out.push_str(digits);
    } else if dp >= len {
        out.push_str(digits);
        for _ in 0..(dp - len) {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..dp as usize]);
        out.push('.');
        out.push_str(&digits[dp as usize..]);
    }
}

/// Formats a float like Go's `encoding/json` (`floatEncoder` for 64-bit floats).
pub fn format_float(f: f64, out: &mut String) {
    if f == 0.0 {
        out.push_str(if f.is_sign_negative() { "-0" } else { "0" });
        return;
    }
    if !f.is_finite() {
        // Go refuses to encode them; never produced by this program.
        out.push_str("null");
        return;
    }
    let abs = f.abs();
    let (negative, digits, exp) = shortest_digits(f);
    if negative {
        out.push('-');
    }
    if !(1e-6..1e21).contains(&abs) {
        // 'e' format: d[.ddd]e±XX, then Go strips the leading zero of a negative
        // two-digit exponent (e-07 → e-7).
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&exp.abs().to_string());
    } else {
        push_plain(out, &digits, exp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(x: f64) -> String {
        let mut s = String::new();
        format_float(x, &mut s);
        s
    }

    #[test]
    fn floats_like_go() {
        assert_eq!(f(1.0), "1");
        assert_eq!(f(-0.0), "-0");
        assert_eq!(f(0.1), "0.1");
        assert_eq!(f(1e-7), "1e-7");
        assert_eq!(f(1.5e-7), "1.5e-7");
        assert_eq!(f(1e-6), "0.000001");
        assert_eq!(f(1e21), "1e+21");
        assert_eq!(f(1e20), "100000000000000000000");
        assert_eq!(f(12345678901234567890.0), "12345678901234567000");
        assert_eq!(f(123.456), "123.456");
        assert_eq!(f(-2.5e300), "-2.5e+300");
        assert_eq!(f(3.0e-10), "3e-10");
    }

    #[test]
    fn html_safe_escaping() {
        let input: String = [
            'a',
            '<',
            'b',
            '>',
            '&',
            'c',
            '\u{2028}',
            '\u{2029}',
            '\u{1}',
            '\u{7f}',
            '"',
            char::from(92),
            '\n',
        ]
        .iter()
        .collect();
        let v = json!({ "t": input });
        // Expected escapes, built with an explicit backslash to keep the source readable.
        let bs = "\\";
        let expected = [
            "{\"t\":\"a",
            bs,
            "u003cb",
            bs,
            "u003e",
            bs,
            "u0026c",
            bs,
            "u2028",
            bs,
            "u2029",
            bs,
            "u0001",
            "\u{7f}",
            bs,
            "\"",
            bs,
            bs,
            bs,
            "n\"}",
        ]
        .concat();
        assert_eq!(String::from_utf8(to_vec(&v)).unwrap(), expected);
    }

    #[test]
    fn map_keys_sorted_and_compact() {
        let v = json!({"b": 1, "a": [1.0, 2.5], "c": null});
        assert_eq!(
            String::from_utf8(to_vec(&v)).unwrap(),
            r#"{"a":[1,2.5],"b":1,"c":null}"#
        );
    }
}
