//! Go `strconv.Quote` and `strconv.QuoteRune` (the `%q` of a string or a rune, and the `%#v`
//! of a string).

use std::fmt::Write;
use std::sync::OnceLock;

/// Go `strconv.IsPrint`: letters, marks, numbers, punctuation, symbols and the ASCII space,
/// i.e. not in the categories C (controls, format, private use, unassigned) and Z.
fn is_print(c: char) -> bool {
    static NOT_PRINT: OnceLock<regex::Regex> = OnceLock::new();
    if c.is_ascii() {
        return (' '..='~').contains(&c);
    }
    let re = NOT_PRINT.get_or_init(|| regex::Regex::new(r"[\p{C}\p{Z}]").expect("valid regex"));
    !re.is_match(c.encode_utf8(&mut [0; 4]))
}

/// `appendEscapedRune` with `quote` as the quote character.
fn push_escaped(out: &mut String, c: char, quote: char) {
    match c {
        c if c == quote || c == '\\' => {
            out.push('\\');
            out.push(c);
        }
        c if is_print(c) => out.push(c),
        '\u{7}' => out.push_str("\\a"),
        '\u{8}' => out.push_str("\\b"),
        '\u{c}' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{b}' => out.push_str("\\v"),
        c if (c as u32) < 0x20 || c == '\u{7f}' => {
            let _ = write!(out, "\\x{:02x}", c as u32);
        }
        c if (c as u32) < 0x10000 => {
            let _ = write!(out, "\\u{:04x}", c as u32);
        }
        c => {
            let _ = write!(out, "\\U{:08x}", c as u32);
        }
    }
}

/// Go `strconv.Quote` of a string.
pub fn quote(s: &str) -> String {
    quote_bytes(s.as_bytes())
}

/// Go `strconv.Quote` of a Go string holding arbitrary bytes: invalid UTF-8 bytes are
/// written `\xNN`.
pub fn quote_bytes(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() + 2);
    out.push('"');
    for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            push_escaped(&mut out, c, '"');
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out.push('"');
    out
}

/// Go `strconv.QuoteRune`.
pub fn quote_rune(c: char) -> String {
    let mut out = String::from("'");
    push_escaped(&mut out, c, '\'');
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_strconv_quote() {
        assert_eq!(quote("abc"), "\"abc\"");
        assert_eq!(quote("a\"b\\c\n\t'"), "\"a\\\"b\\\\c\\n\\t'\"");
        assert_eq!(quote("\u{1}\u{7f}é~ "), "\"\\x01\\x7fé~ \"");
        assert_eq!(
            quote("\u{85}\u{a0}\u{ad}\u{feff}\u{e000}\u{378}"),
            "\"\\u0085\\u00a0\\u00ad\\ufeff\\ue000\\u0378\""
        );
        assert_eq!(
            quote("\u{e0001}\u{1f600}\u{fffd}"),
            "\"\\U000e0001\u{1f600}\u{fffd}\""
        );
        assert_eq!(quote_bytes(b"a\xffb\xc3"), "\"a\\xffb\\xc3\"");
    }

    #[test]
    fn like_strconv_quote_rune() {
        assert_eq!(quote_rune('a'), "'a'");
        assert_eq!(quote_rune('\''), "'\\''");
        assert_eq!(quote_rune('"'), "'\"'");
        assert_eq!(quote_rune('\\'), "'\\\\'");
        assert_eq!(quote_rune('\0'), "'\\x00'");
        assert_eq!(quote_rune('\u{a0}'), "'\\u00a0'");
        assert_eq!(quote_rune('é'), "'é'");
    }
}
