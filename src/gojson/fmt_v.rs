use super::encode::{push_plain, shortest_digits};
use serde_json::Value;

/// Formats a float like Go's `%v` (`strconv.FormatFloat(f, 'g', -1, 64)`): shortest digits,
/// exponent form (at least two exponent digits) when the exponent is below -4 or at least 6,
/// since Go uses precision 6 for that decision with the shortest representation.
pub fn fmt_float_v(f: f64, out: &mut String) {
    if f == 0.0 {
        out.push_str(if f.is_sign_negative() { "-0" } else { "0" });
        return;
    }
    if f.is_nan() {
        out.push_str("NaN");
        return;
    }
    if f.is_infinite() {
        out.push_str(if f > 0.0 { "+Inf" } else { "-Inf" });
        return;
    }
    let (negative, digits, exp) = shortest_digits(f);
    if negative {
        out.push('-');
    }
    if !(-4..6).contains(&exp) {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let e = exp.abs();
        if e < 10 {
            out.push('0');
        }
        out.push_str(&e.to_string());
    } else {
        push_plain(out, &digits, exp);
    }
}

/// Formats a value decoded from JSON like Go's `fmt.Sprintf("%v", v)` on the
/// `interface{}` produced by `json.Unmarshal` (numbers are `float64`, objects are maps printed
/// with sorted keys).
pub fn fmt_v(v: &Value) -> String {
    let mut out = String::new();
    write_v(v, &mut out);
    out
}

fn write_v(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("<nil>"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => fmt_float_v(n.as_f64().unwrap_or(0.0), out),
        Value::String(s) => out.push_str(s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_v(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push_str("map[");
            // serde_json maps are sorted by key, like Go's fmt prints maps.
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(k);
                out.push(':');
                write_v(item, out);
            }
            out.push(']');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_like_go() {
        assert_eq!(fmt_v(&json!(null)), "<nil>");
        assert_eq!(fmt_v(&json!(42)), "42");
        assert_eq!(fmt_v(&json!(1.5)), "1.5");
        assert_eq!(fmt_v(&json!(1000000)), "1e+06");
        assert_eq!(fmt_v(&json!(123456)), "123456");
        assert_eq!(fmt_v(&json!(0.0001)), "0.0001");
        assert_eq!(fmt_v(&json!(0.00001)), "1e-05");
        assert_eq!(fmt_v(&json!(1234567.0)), "1.234567e+06");
        assert_eq!(fmt_v(&json!("abc")), "abc");
        assert_eq!(fmt_v(&json!([1, "a", true])), "[1 a true]");
        assert_eq!(
            fmt_v(&json!({"b": {"y": 2}, "a": [], "c": null})),
            "map[a:[] b:map[y:2] c:<nil>]"
        );
    }
}
