//! Go `net/mail.ParseAddress` (Go 1.25), for the `email` and `idn-email` formats of the JSON
//! schema: same accepted addresses and same error messages.
//!
//! RFC 2047 encoded words matter only through their errors: a well-formed word with a charset
//! other than UTF-8, ISO-8859-1 and US-ASCII fails, as Go's parser has no charset reader. The
//! decoded text is used for the choice between the two "angle-addr" messages.

use crate::gojson::{quote, quote_rune};

struct Parser<'a> {
    s: &'a str,
}

fn is_vchar(r: char) -> bool {
    ('!'..='~').contains(&r) || !r.is_ascii()
}

fn is_wsp(r: char) -> bool {
    r == ' ' || r == '\t'
}

fn is_atext(r: char, dot: bool) -> bool {
    match r {
        '.' => dot,
        '(' | ')' | '<' | '>' | '[' | ']' | ':' | ';' | '@' | '\\' | ',' | '"' => false,
        r => is_vchar(r),
    }
}

fn is_qtext(r: char) -> bool {
    r != '\\' && r != '"' && is_vchar(r)
}

fn is_dtext(r: char) -> bool {
    r != '[' && r != ']' && r != '\\' && is_vchar(r)
}

/// `base64.StdEncoding.DecodeString` (padding required, CR and LF ignored).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let data: Vec<u8> = s.bytes().filter(|&c| c != b'\r' && c != b'\n').collect();
    if !data.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    for (n, quad) in data.chunks(4).enumerate() {
        let last = n == data.len() / 4 - 1;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut acc = 0u32;
        for &c in &quad[..4 - pad] {
            acc = (acc << 6) | val(c)?;
        }
        // StdEncoding is not strict: bits after the last character are ignored.
        match pad {
            0 => out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]),
            1 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
            _ => out.push((acc >> 4) as u8),
        }
    }
    Some(out)
}

/// `qDecode` of `mime`.
fn q_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => out.push(b' '),
            b'=' => {
                if i + 2 >= b.len() {
                    return None;
                }
                let h = (b[i + 1] as char).to_digit(16)?;
                let l = (b[i + 2] as char).to_digit(16)?;
                out.push((h * 16 + l) as u8);
                i += 2;
            }
            c if (b' '..=b'~').contains(&c) || matches!(c, b'\n' | b'\r' | b'\t') => out.push(c),
            _ => return None,
        }
        i += 1;
    }
    Some(out)
}

/// `decodeRFC2047Word`: (word, is_encoded), or the charset error.
fn decode_rfc2047_word(s: &str) -> Result<(String, bool), String> {
    let not_encoded = Ok((s.to_string(), false));
    if s.len() < 8 || !s.starts_with("=?") || !s.ends_with("?=") || s.matches('?').count() != 4 {
        return not_encoded;
    }
    let word = &s[2..s.len() - 2];
    let (charset, text) = word.split_once('?').unwrap_or((word, ""));
    if charset.is_empty() {
        return not_encoded;
    }
    let (encoding, text) = text.split_once('?').unwrap_or((text, ""));
    let content = match encoding {
        "B" | "b" => base64_decode(text),
        "Q" | "q" => q_decode(text),
        _ => None,
    };
    let Some(content) = content else {
        return not_encoded;
    };
    if charset.eq_ignore_ascii_case("utf-8") {
        Ok((String::from_utf8_lossy(&content).into_owned(), true))
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Ok((content.iter().map(|&c| c as char).collect(), true))
    } else if charset.eq_ignore_ascii_case("us-ascii") {
        let text = content
            .iter()
            .map(|&c| if c.is_ascii() { c as char } else { '\u{FFFD}' })
            .collect();
        Ok((text, true))
    } else {
        Err(format!(
            "charset not supported: {}",
            quote(&charset.to_lowercase())
        ))
    }
}

impl<'a> Parser<'a> {
    fn consume(&mut self, c: u8) -> bool {
        if self.s.as_bytes().first() == Some(&c) {
            self.s = &self.s[1..];
            true
        } else {
            false
        }
    }

    fn skip_space(&mut self) {
        self.s = self.s.trim_start_matches([' ', '\t']);
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().first().copied()
    }

    /// `skipCFWS`: false on an unclosed comment.
    fn skip_cfws(&mut self) -> bool {
        self.skip_space();
        while self.consume(b'(') {
            if self.consume_comment().is_none() {
                return false;
            }
            self.skip_space();
        }
        true
    }

    /// `consumeComment` (after the `(`): the comment text, `None` when unclosed.
    fn consume_comment(&mut self) -> Option<String> {
        let mut depth = 1;
        let mut comment = Vec::new();
        let b = self.s.as_bytes();
        let mut i = 0;
        while i < b.len() && depth > 0 {
            if b[i] == b'\\' && b.len() - i > 1 {
                i += 1;
            } else if b[i] == b'(' {
                depth += 1;
            } else if b[i] == b')' {
                depth -= 1;
            }
            if depth > 0 {
                comment.push(b[i]);
            }
            i += 1;
        }
        // `i` may stop inside a character only when the string ends there.
        self.s = self.s.get(i..).unwrap_or("");
        (depth == 0).then(|| String::from_utf8_lossy(&comment).into_owned())
    }

    fn consume_quoted_string(&mut self) -> Result<String, String> {
        let mut out = String::new();
        let mut escaped = false;
        let mut chars = self.s.char_indices().skip(1);
        loop {
            let Some((i, r)) = chars.next() else {
                return Err("mail: unclosed quoted-string".to_string());
            };
            if escaped {
                if !is_vchar(r) && !is_wsp(r) {
                    return Err(format!(
                        "mail: bad character in quoted-string: {}",
                        quote_rune(r)
                    ));
                }
                out.push(r);
                escaped = false;
            } else if is_qtext(r) || is_wsp(r) {
                out.push(r);
            } else if r == '"' {
                self.s = &self.s[i + 1..];
                return Ok(out);
            } else if r == '\\' {
                escaped = true;
            } else {
                return Err(format!(
                    "mail: bad character in quoted-string: {}",
                    quote_rune(r)
                ));
            }
        }
    }

    fn consume_atom(&mut self, dot: bool, permissive: bool) -> Result<&'a str, String> {
        let end = self
            .s
            .char_indices()
            .find(|(_, r)| !is_atext(*r, dot))
            .map_or(self.s.len(), |(i, _)| i);
        if end == 0 {
            return Err("mail: invalid string".to_string());
        }
        let atom = &self.s[..end];
        self.s = &self.s[end..];
        if !permissive {
            if atom.starts_with('.') {
                return Err("mail: leading dot in atom".to_string());
            }
            if atom.contains("..") {
                return Err("mail: double dot in atom".to_string());
            }
            if atom.ends_with('.') {
                return Err("mail: trailing dot in atom".to_string());
            }
        }
        Ok(atom)
    }

    fn consume_domain_literal(&mut self) -> Result<(), String> {
        if !self.consume(b'[') {
            return Err("mail: missing \"[\" in domain-literal".to_string());
        }
        let start = self.s;
        let mut len = 0;
        loop {
            let Some(r) = self.s.chars().next() else {
                return Err("mail: unclosed domain-literal".to_string());
            };
            if r == ']' {
                break;
            }
            if !is_dtext(r) {
                return Err(format!(
                    "mail: bad character in domain-literal: {}",
                    quote_rune(r)
                ));
            }
            len += r.len_utf8();
            self.s = &self.s[r.len_utf8()..];
        }
        let dtext = &start[..len];
        self.consume(b']');
        if !crate::gourl::is_ip(dtext) {
            return Err(format!(
                "mail: invalid IP address in domain-literal: {}",
                quote(dtext)
            ));
        }
        Ok(())
    }

    /// `consumeAddrSpec`; the parser is left unchanged on error.
    fn consume_addr_spec(&mut self) -> Result<(), String> {
        let orig = self.s;
        let r = self.addr_spec();
        if r.is_err() {
            self.s = orig;
        }
        r
    }

    fn addr_spec(&mut self) -> Result<(), String> {
        self.skip_space();
        match self.peek() {
            None => return Err("mail: no addr-spec".to_string()),
            Some(b'"') => {
                // An error leaves the local part empty, so Go reports the empty string.
                if self.consume_quoted_string().unwrap_or_default().is_empty() {
                    return Err("mail: empty quoted string in addr-spec".to_string());
                }
            }
            Some(_) => {
                self.consume_atom(true, false)?;
            }
        }
        if !self.consume(b'@') {
            return Err("mail: missing @ in addr-spec".to_string());
        }
        self.skip_space();
        match self.peek() {
            None => Err("mail: no domain in addr-spec".to_string()),
            Some(b'[') => self.consume_domain_literal(),
            Some(_) => self.consume_atom(true, false).map(drop),
        }
    }

    fn consume_phrase(&mut self) -> Result<String, String> {
        let mut words: Vec<String> = Vec::new();
        let mut encoded = String::new();
        let mut err = None;
        loop {
            if !words.is_empty() && !self.skip_cfws() {
                return Err("mail: misformatted parenthetical comment".to_string());
            }
            self.skip_space();
            if self.s.is_empty() {
                break;
            }
            let word = if self.peek() == Some(b'"') {
                self.consume_quoted_string().map(|w| (w, false))
            } else {
                self.consume_atom(true, true).and_then(decode_rfc2047_word)
            };
            match word {
                Err(e) => {
                    err = Some(e);
                    break;
                }
                Ok((w, true)) => encoded.push_str(&w),
                Ok((w, false)) => {
                    if !encoded.is_empty() {
                        words.push(std::mem::take(&mut encoded));
                    }
                    words.push(w);
                }
            }
        }
        if !encoded.is_empty() {
            words.push(encoded);
        }
        match err {
            Some(e) if words.is_empty() => Err(format!("mail: missing word in phrase: {e}")),
            _ => Ok(words.join(" ")),
        }
    }

    fn consume_display_name_comment(&mut self) -> Result<(), String> {
        self.consume(b'(');
        let Some(comment) = self.consume_comment() else {
            return Err("mail: misformatted parenthetical comment".to_string());
        };
        for word in comment.split([' ', '\t']).filter(|w| !w.is_empty()) {
            decode_rfc2047_word(word)?;
        }
        Ok(())
    }

    /// `parseAddress`: the number of addresses parsed.
    fn parse_address(&mut self, handle_group: bool) -> Result<usize, String> {
        self.skip_space();
        if self.s.is_empty() {
            return Err("mail: no address".to_string());
        }
        if self.consume_addr_spec().is_ok() {
            self.skip_space();
            if self.peek() == Some(b'(') {
                self.consume_display_name_comment()?;
            }
            return Ok(1);
        }
        let mut display_name = String::new();
        if self.peek() != Some(b'<') {
            display_name = self.consume_phrase()?;
        }
        self.skip_space();
        if handle_group && self.consume(b':') {
            return self.consume_group_list();
        }
        if !self.consume(b'<') {
            return Err(if display_name.chars().all(|r| is_atext(r, true)) {
                "mail: missing '@' or angle-addr".to_string()
            } else {
                "mail: no angle-addr".to_string()
            });
        }
        self.consume_addr_spec()?;
        if !self.consume(b'>') {
            return Err("mail: unclosed angle-addr".to_string());
        }
        Ok(1)
    }

    fn consume_group_list(&mut self) -> Result<usize, String> {
        const COMMENT: &str = "mail: misformatted parenthetical comment";
        self.skip_space();
        if self.consume(b';') {
            return if self.skip_cfws() {
                Ok(0)
            } else {
                Err(COMMENT.to_string())
            };
        }
        let mut n = 0;
        loop {
            self.skip_space();
            n += self.parse_address(false)?;
            if !self.skip_cfws() {
                return Err(COMMENT.to_string());
            }
            if self.consume(b';') {
                if !self.skip_cfws() {
                    return Err(COMMENT.to_string());
                }
                return Ok(n);
            }
            if !self.consume(b',') {
                return Err("mail: expected comma".to_string());
            }
        }
    }
}

/// Go `mail.ParseAddress(address)`: `Err` holds Go's message.
pub fn parse_address(address: &str) -> Result<(), String> {
    let mut p = Parser { s: address };
    let n = p.parse_address(true)?;
    if !p.skip_cfws() {
        return Err("mail: misformatted parenthetical comment".to_string());
    }
    if !p.s.is_empty() {
        return Err(format!("mail: expected single address, got {}", quote(p.s)));
    }
    match n {
        0 => Err("mail: empty group".to_string()),
        1 => Ok(()),
        _ => Err("mail: group with multiple addresses".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_like_go() {
        for a in [
            "a@b.c",
            "a@b",
            " a@b ",
            "Barry Gibbs <bg@example.com>",
            "\"Barry Gibbs\" <bg@example.com>",
            "<bg@example.com>",
            "a@b (comment)",
            "\"quoted local\"@b",
            "a@[127.0.0.1]",
            "a@[::1]",
            "g: a@b;",
            "=?utf-8?q?J=C3=B6rg?= <j@b>",
            "=?iso-8859-1?q?J=F6rg?= <j@b>",
            "=?x-unknown?q?a?=@b",
            "é@b.c",
            "a@b (c (nested) d)",
        ] {
            assert_eq!(parse_address(a), Ok(()), "{a}");
        }
    }

    #[test]
    fn errors_like_go() {
        for (a, e) in [
            ("", "mail: no address"),
            ("a", "mail: missing '@' or angle-addr"),
            ("a b", "mail: no angle-addr"),
            ("a@", "mail: missing '@' or angle-addr"),
            ("a@b.", "mail: missing '@' or angle-addr"),
            ("a..b@c", "mail: missing '@' or angle-addr"),
            ("<a@b", "mail: unclosed angle-addr"),
            ("<a>", "mail: missing @ in addr-spec"),
            ("<>", "mail: invalid string"),
            ("a@b c", "mail: expected single address, got \"c\""),
            ("a@b (c", "mail: misformatted parenthetical comment"),
            ("g: a@b, c@d;", "mail: group with multiple addresses"),
            ("g:;", "mail: empty group"),
            ("g: a@b c@d;", "mail: expected comma"),
            (
                "\"a",
                "mail: missing word in phrase: mail: unclosed quoted-string",
            ),
            (
                "\"a\u{1}\" <a@b>",
                "mail: missing word in phrase: mail: bad character in quoted-string: '\\x01'",
            ),
            ("a@[1.2.3]", "mail: missing '@' or angle-addr"),
            (
                "<a@[1.2.3]>",
                "mail: invalid IP address in domain-literal: \"1.2.3\"",
            ),
            ("a@[a\\b]", "mail: missing '@' or angle-addr"),
            ("<a@[a[b]>", "mail: bad character in domain-literal: '['"),
            ("<a@[1.2", "mail: unclosed domain-literal"),
            ("\"\"@b", "mail: missing '@' or angle-addr"),
            ("<\"\"@b>", "mail: empty quoted string in addr-spec"),
            (
                "=?x-unknown?q?a?= <a@b>",
                "mail: missing word in phrase: charset not supported: \"x-unknown\"",
            ),
            ("a@b (=?X?q?a?=)", "charset not supported: \"x\""),
            (",", "mail: missing word in phrase: mail: invalid string"),
            (
                "]]02@2000190000",
                "mail: missing word in phrase: mail: invalid string",
            ),
        ] {
            assert_eq!(parse_address(a).unwrap_err(), e, "{a}");
        }
    }

    #[test]
    fn rfc2047_decoding() {
        assert_eq!(base64_decode("SsO2cmc="), Some("Jörg".as_bytes().to_vec()));
        assert_eq!(base64_decode("YQ=="), Some(b"a".to_vec()));
        assert_eq!(base64_decode("YR=="), Some(b"a".to_vec()));
        assert_eq!(base64_decode("Y=Q="), None);
        assert_eq!(base64_decode("YQ="), None);
        assert_eq!(base64_decode("Y==="), None);
        assert_eq!(q_decode("a_b=3D"), Some(b"a b=".to_vec()));
        assert_eq!(q_decode("a=3"), None);
        assert_eq!(
            decode_rfc2047_word("=?UTF-8?B?SsO2cmc=?="),
            Ok(("Jörg".to_string(), true))
        );
        assert_eq!(
            decode_rfc2047_word("=?us-ascii?q?a=FF?="),
            Ok(("a\u{FFFD}".to_string(), true))
        );
        assert_eq!(
            decode_rfc2047_word("=?utf-8?x?a?="),
            Ok(("=?utf-8?x?a?=".to_string(), false))
        );
    }
}
