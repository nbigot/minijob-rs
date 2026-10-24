//! The checks of Go's `url.Parse` (Go 1.25 `net/url`, with `net/netip` for IP literals), for
//! the `uri`, `uri-reference`, `iri`, `iri-reference` and `uri-template` formats of the JSON
//! schema: same accepted strings and same error messages, without building a URL.
//!
//! Go strings may hold any bytes once percent-decoded, so hosts are handled as bytes.

use crate::gojson::{quote, quote_bytes};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Path,
    Host,
    Zone,
    UserPassword,
    Fragment,
    QueryComponent,
}

fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// `shouldEscape` (the modes `url.Parse` uses).
fn should_escape(c: u8, mode: Mode) -> bool {
    if is_alnum(c) {
        return false;
    }
    if matches!(mode, Mode::Host | Mode::Zone)
        && matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
        )
    {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => return false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Mode::Path => return c == b'?',
            Mode::UserPassword => return matches!(c, b'@' | b'/' | b'?' | b':'),
            Mode::Fragment => return false,
            Mode::Host | Mode::Zone | Mode::QueryComponent => {}
        },
        _ => {}
    }
    if mode == Mode::Fragment && matches!(c, b'!' | b'(' | b')' | b'*') {
        return false;
    }
    true
}

fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

/// `unescape`: checks the escapes (and the host characters), returns the decoded bytes.
fn unescape(s: &[u8], mode: Mode) -> Result<Vec<u8>, String> {
    let escape_error = |e: &[u8]| format!("invalid URL escape {}", quote_bytes(e));
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'%' => {
                if i + 2 >= s.len()
                    || !s[i + 1].is_ascii_hexdigit()
                    || !s[i + 2].is_ascii_hexdigit()
                {
                    return Err(escape_error(&s[i..s.len().min(i + 3)]));
                }
                let esc = &s[i..i + 3];
                if mode == Mode::Host && unhex(s[i + 1]) < 8 && esc != b"%25" {
                    return Err(escape_error(esc));
                }
                if mode == Mode::Zone {
                    let v = (unhex(s[i + 1]) << 4) | unhex(s[i + 2]);
                    if esc != b"%25" && v != b' ' && should_escape(v, Mode::Host) {
                        return Err(escape_error(esc));
                    }
                }
                i += 3;
            }
            c => {
                if matches!(mode, Mode::Host | Mode::Zone) && c < 0x80 && should_escape(c, mode) {
                    return Err(format!(
                        "invalid character {} in host name",
                        quote_bytes(&s[i..=i])
                    ));
                }
                i += 1;
            }
        }
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            out.push((unhex(s[i + 1]) << 4) | unhex(s[i + 2]));
            i += 3;
        } else {
            out.push(if mode == Mode::QueryComponent && s[i] == b'+' {
                b' '
            } else {
                s[i]
            });
            i += 1;
        }
    }
    Ok(out)
}

/// `url.QueryUnescape`: decodes the `%XX` escapes and `+` (a space).
pub fn query_unescape(s: &str) -> Result<Vec<u8>, String> {
    unescape(s.as_bytes(), Mode::QueryComponent)
}

/// `getScheme`: the scheme (possibly empty) and the rest.
fn get_scheme(raw: &str) -> Result<(&str, &str), String> {
    for (i, c) in raw.bytes().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if i == 0 => return Ok(("", raw)),
            b'0'..=b'9' | b'+' | b'-' | b'.' => {}
            b':' if i == 0 => return Err("missing protocol scheme".to_string()),
            b':' => return Ok((&raw[..i], &raw[i + 1..])),
            _ => return Ok(("", raw)),
        }
    }
    Ok(("", raw))
}

fn valid_optional_port(port: &str) -> bool {
    match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(digits) => digits.bytes().all(|b| b.is_ascii_digit()),
    }
}

fn valid_userinfo(s: &str) -> bool {
    s.chars().all(|r| {
        r.is_ascii_alphanumeric()
            || matches!(
                r,
                '-' | '.'
                    | '_'
                    | ':'
                    | '~'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
                    | '@'
            )
    })
}

/// `parseHost`: the host as Go stores it in `URL.Host` (decoded, with the port).
fn parse_host(host: &str) -> Result<Vec<u8>, String> {
    match host.rfind('[') {
        Some(open) if open > 0 => return Err("invalid IP-literal".to_string()),
        Some(_) => {
            let Some(close) = host.rfind(']') else {
                return Err("missing ']' in host".to_string());
            };
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port) {
                return Err(format!("invalid port {} after host", quote(colon_port)));
            }
            let port = unescape(colon_port.as_bytes(), Mode::Host)?;
            let hostname = &host.as_bytes()[1..close];
            let unescaped = match find(hostname, b"%25") {
                Some(z) => {
                    let mut h = unescape(&hostname[..z], Mode::Host)?;
                    h.extend(unescape(&hostname[z..], Mode::Zone)?);
                    h
                }
                None => unescape(hostname, Mode::Host)?,
            };
            return match parse_addr(&unescaped) {
                Err(e) => Err(format!("invalid host: {e}")),
                Ok(Addr::V4) => Err("invalid IP-literal".to_string()),
                Ok(Addr::V6) => Ok([b"[", &unescaped[..], b"]", &port[..]].concat()),
            };
        }
        None => {
            if let Some(i) = host.rfind(':') {
                let colon_port = &host[i..];
                if !valid_optional_port(colon_port) {
                    return Err(format!("invalid port {} after host", quote(colon_port)));
                }
            }
        }
    }
    unescape(host.as_bytes(), Mode::Host)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `parseAuthority`: the host.
fn parse_authority(authority: &str) -> Result<Vec<u8>, String> {
    let at = authority.rfind('@');
    let host = parse_host(match at {
        Some(i) => &authority[i + 1..],
        None => authority,
    })?;
    let Some(i) = at else {
        return Ok(host);
    };
    let userinfo = &authority[..i];
    if !valid_userinfo(userinfo) {
        return Err("net/url: invalid userinfo".to_string());
    }
    match userinfo.split_once(':') {
        None => unescape(userinfo.as_bytes(), Mode::UserPassword)?,
        Some((user, password)) => {
            unescape(user.as_bytes(), Mode::UserPassword)?;
            unescape(password.as_bytes(), Mode::UserPassword)?
        }
    };
    Ok(host)
}

/// The fields of Go's `url.URL` that callers use (decoded values are bytes, as in Go).
#[derive(Debug, Default, PartialEq)]
pub struct Url {
    /// Lowercase.
    pub scheme: String,
    pub opaque: String,
    pub host: Vec<u8>,
    pub path: Vec<u8>,
    pub raw_query: String,
    pub fragment: Vec<u8>,
}

/// `parse(rawURL, false)`.
fn parse_reference(raw: &str) -> Result<Url, String> {
    if raw.bytes().any(|b| b < b' ' || b == 0x7f) {
        return Err("net/url: invalid control character in URL".to_string());
    }
    let mut url = Url::default();
    if raw == "*" {
        url.path = b"*".to_vec();
        return Ok(url);
    }
    let (scheme, rest) = get_scheme(raw)?;
    url.scheme = scheme.to_ascii_lowercase();
    let rest = if rest.ends_with('?') && rest.matches('?').count() == 1 {
        &rest[..rest.len() - 1]
    } else {
        match rest.split_once('?') {
            Some((r, q)) => {
                url.raw_query = q.to_string();
                r
            }
            None => rest,
        }
    };
    if !rest.starts_with('/') {
        if !scheme.is_empty() {
            // Opaque: nothing else is checked.
            url.opaque = rest.to_string();
            return Ok(url);
        }
        let segment = rest.split_once('/').map_or(rest, |(s, _)| s);
        if segment.contains(':') {
            return Err("first path segment in URL cannot contain colon".to_string());
        }
    }
    let mut path = rest;
    if (!scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
        let authority = &rest[2..];
        let (authority, tail) = match authority.find('/') {
            Some(i) => (&authority[..i], &authority[i..]),
            None => (authority, ""),
        };
        url.host = parse_authority(authority)?;
        path = tail;
    }
    url.path = unescape(path.as_bytes(), Mode::Path)?;
    Ok(url)
}

/// Go `url.Parse(raw)`: `Err` holds the message of the `*url.Error`.
pub fn parse(raw: &str) -> Result<Url, String> {
    let (u, frag) = raw.split_once('#').unwrap_or((raw, ""));
    let mut url = parse_reference(u).map_err(|e| format!("parse {}: {e}", quote(u)))?;
    if !frag.is_empty() {
        url.fragment = unescape(frag.as_bytes(), Mode::Fragment)
            .map_err(|e| format!("parse {}: {e}", quote(raw)))?;
    }
    Ok(url)
}

// ------------------------------------------------------------------ net/netip

/// Go `net.ParseIP(s) != nil`: an IPv4 or IPv6 address, without zone.
pub fn is_ip(s: &str) -> bool {
    !s.contains('%') && parse_addr(s.as_bytes()).is_ok()
}

enum Addr {
    V4,
    V6,
}

/// `parseAddrError.Error()`.
fn addr_error(input: &[u8], msg: &str, at: &[u8]) -> String {
    if at.is_empty() {
        format!("ParseAddr({}): {msg}", quote_bytes(input))
    } else {
        format!(
            "ParseAddr({}): {msg} (at {})",
            quote_bytes(input),
            quote_bytes(at)
        )
    }
}

/// `netip.ParseAddr`, the kind of address only.
fn parse_addr(s: &[u8]) -> Result<Addr, String> {
    for &c in s {
        match c {
            b'.' => return parse_ipv4_fields(s, 0, s.len()).map(|_| Addr::V4),
            b':' => return parse_ipv6(s).map(|_| Addr::V6),
            b'%' => return Err(addr_error(s, "missing IPv6 address", b"")),
            _ => {}
        }
    }
    Err(addr_error(s, "unable to parse IP", b""))
}

/// `parseIPv4Fields` on `input[off..end]`.
fn parse_ipv4_fields(input: &[u8], off: usize, end: usize) -> Result<(), String> {
    let s = &input[off..end];
    let (mut val, mut pos, mut digits) = (0u32, 0, 0);
    for (i, &c) in s.iter().enumerate() {
        if c.is_ascii_digit() {
            if digits == 1 && val == 0 {
                return Err(addr_error(
                    input,
                    "IPv4 field has octet with leading zero",
                    b"",
                ));
            }
            val = val * 10 + u32::from(c - b'0');
            digits += 1;
            if val > 255 {
                return Err(addr_error(input, "IPv4 field has value >255", b""));
            }
        } else if c == b'.' {
            if i == 0 || i == s.len() - 1 || s[i - 1] == b'.' {
                return Err(addr_error(
                    input,
                    "IPv4 field must have at least one digit",
                    &s[i..],
                ));
            }
            if pos == 3 {
                return Err(addr_error(input, "IPv4 address too long", b""));
            }
            pos += 1;
            val = 0;
            digits = 0;
        } else {
            return Err(addr_error(input, "unexpected character", &s[i..]));
        }
    }
    if pos < 3 {
        return Err(addr_error(input, "IPv4 address too short", b""));
    }
    Ok(())
}

/// `parseIPv6`.
fn parse_ipv6(input: &[u8]) -> Result<(), String> {
    let mut s = input;
    let mut zone: &[u8] = b"";
    if let Some(i) = s.iter().position(|&c| c == b'%') {
        (s, zone) = (&s[..i], &s[i + 1..]);
        if zone.is_empty() {
            return Err(addr_error(input, "zone must be a non-empty string", b""));
        }
    }
    let mut ellipsis: Option<usize> = None;
    if s.len() >= 2 && s[0] == b':' && s[1] == b':' {
        ellipsis = Some(0);
        s = &s[2..];
        if s.is_empty() {
            return Ok(());
        }
    }
    let mut i = 0;
    while i < 16 {
        let mut off = 0;
        let mut acc: u32 = 0;
        while off < s.len() {
            let c = s[off];
            let v = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => break,
            };
            acc = (acc << 4) + u32::from(v);
            if off > 3 {
                return Err(addr_error(
                    input,
                    "each group must have 4 or less digits",
                    s,
                ));
            }
            if acc > 0xFFFF {
                return Err(addr_error(input, "IPv6 field has value >=2^16", s));
            }
            off += 1;
        }
        if off == 0 {
            return Err(addr_error(
                input,
                "each colon-separated field must have at least one digit",
                s,
            ));
        }
        if off < s.len() && s[off] == b'.' {
            if ellipsis.is_none() && i != 12 {
                return Err(addr_error(
                    input,
                    "embedded IPv4 address must replace the final 2 fields of the address",
                    s,
                ));
            }
            if i + 4 > 16 {
                return Err(addr_error(
                    input,
                    "too many hex fields to fit an embedded IPv4 at the end of the address",
                    s,
                ));
            }
            let end = if zone.is_empty() {
                input.len()
            } else {
                input.len() - zone.len() - 1
            };
            parse_ipv4_fields(input, end - s.len(), end)?;
            s = b"";
            i += 4;
            break;
        }
        i += 2;
        s = &s[off..];
        if s.is_empty() {
            break;
        }
        if s[0] != b':' {
            return Err(addr_error(input, "unexpected character, want colon", s));
        } else if s.len() == 1 {
            return Err(addr_error(
                input,
                "colon must be followed by more characters",
                s,
            ));
        }
        s = &s[1..];
        if s[0] == b':' {
            if ellipsis.is_some() {
                return Err(addr_error(input, "multiple :: in address", s));
            }
            ellipsis = Some(i);
            s = &s[1..];
            if s.is_empty() {
                break;
            }
        }
    }
    if !s.is_empty() {
        return Err(addr_error(input, "trailing garbage after address", s));
    }
    if i < 16 {
        if ellipsis.is_none() {
            return Err(addr_error(input, "address string too short", b""));
        }
    } else if ellipsis.is_some() {
        return Err(addr_error(
            input,
            "the :: must expand to at least one field of zeros",
            b"",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_like_go() {
        for u in [
            "",
            "*",
            "http://a/b",
            "HTTP://user:p%40ss@host.example:8080/p%20a?q=%zz#f",
            "mailto:a@b.c",
            "urn:isbn:0451450523",
            "/a/b:c",
            "a/b:c",
            "a%2Fb",
            "//host",
            "///a",
            "http://[::1]:80/",
            "http://[fe80::1%25en0]/",
            "http://[::ffff:1.2.3.4]/",
            "http://ex%C3%A9.com/",
            "x?a:b",
            "http://a@b@c/",
            "1a",
            "http://a/b\\c",
            "a#b#c",
        ] {
            assert!(parse(u).is_ok(), "{u}");
        }
    }

    #[test]
    fn errors_like_go() {
        for (u, e) in [
            (
                "2026-10-18T12:00:00Z",
                "parse \"2026-10-18T12:00:00Z\": first path segment in URL cannot contain colon",
            ),
            ("::1", "parse \"::1\": missing protocol scheme"),
            (":", "parse \":\": missing protocol scheme"),
            ("a%zz", "parse \"a%zz\": invalid URL escape \"%zz\""),
            ("a%z", "parse \"a%z\": invalid URL escape \"%z\""),
            ("a%", "parse \"a%\": invalid URL escape \"%\""),
            ("a%é", "parse \"a%é\": invalid URL escape \"%é\""),
            ("a%\u{e9}x", "parse \"a%éx\": invalid URL escape \"%é\""),
            (
                "a%x\u{e9}",
                "parse \"a%xé\": invalid URL escape \"%x\\xc3\"",
            ),
            (
                "http://a b/",
                "parse \"http://a b/\": invalid character \" \" in host name",
            ),
            (
                "a\n",
                "parse \"a\\n\": net/url: invalid control character in URL",
            ),
            ("a#%x", "parse \"a#%x\": invalid URL escape \"%x\""),
            ("x:a#%", "parse \"x:a#%\": invalid URL escape \"%\""),
            (
                "http://a%20b/",
                "parse \"http://a%20b/\": invalid URL escape \"%20\"",
            ),
            (
                "http://a/b/%",
                "parse \"http://a/b/%\": invalid URL escape \"%\"",
            ),
            (
                "http://a:b/",
                "parse \"http://a:b/\": invalid port \":b\" after host",
            ),
            (
                "http://a:8a/",
                "parse \"http://a:8a/\": invalid port \":8a\" after host",
            ),
            (
                "http://a{b/",
                "parse \"http://a{b/\": invalid character \"{\" in host name",
            ),
            (
                "http://a^/",
                "parse \"http://a^/\": invalid character \"^\" in host name",
            ),
            (
                "http://x[::1]/",
                "parse \"http://x[::1]/\": invalid IP-literal",
            ),
            (
                "http://[::1/",
                "parse \"http://[::1/\": missing ']' in host",
            ),
            (
                "http://[::1]x/",
                "parse \"http://[::1]x/\": invalid port \"x\" after host",
            ),
            (
                "http://[1.2.3.4]/",
                "parse \"http://[1.2.3.4]/\": invalid IP-literal",
            ),
            (
                "http://[x]/",
                "parse \"http://[x]/\": invalid host: ParseAddr(\"x\"): unable to parse IP",
            ),
            (
                "http://[::1::2]/",
                "parse \"http://[::1::2]/\": invalid host: ParseAddr(\"::1::2\"): multiple :: in address (at \":2\")",
            ),
            (
                "http://[fe80::1%25]/",
                "parse \"http://[fe80::1%25]/\": invalid host: ParseAddr(\"fe80::1%\"): zone must be a non-empty string",
            ),
            (
                "http://[fe80::1%25a%0a]/",
                "parse \"http://[fe80::1%25a%0a]/\": invalid URL escape \"%0a\"",
            ),
            (
                "http://[%ff::]/",
                "parse \"http://[%ff::]/\": invalid host: ParseAddr(\"\\xff::\"): each colon-separated field must have at least one digit (at \"\\xff::\")",
            ),
            (
                "http://é@a/",
                "parse \"http://é@a/\": net/url: invalid userinfo",
            ),
            (
                "http://a%zz@b/",
                "parse \"http://a%zz@b/\": invalid URL escape \"%zz\"",
            ),
            (
                "http://a:%zz@b/",
                "parse \"http://a:%zz@b/\": invalid URL escape \"%zz\"",
            ),
        ] {
            assert_eq!(parse(u).unwrap_err(), e, "{u}");
        }
    }

    #[test]
    fn netip_messages() {
        let e = |s: &str| parse_addr(s.as_bytes()).err().unwrap_or_default();
        assert_eq!(e("1.2.3"), "ParseAddr(\"1.2.3\"): IPv4 address too short");
        assert_eq!(
            e("1.2.3.4.5"),
            "ParseAddr(\"1.2.3.4.5\"): IPv4 address too long"
        );
        assert_eq!(
            e("1.02.3.4"),
            "ParseAddr(\"1.02.3.4\"): IPv4 field has octet with leading zero"
        );
        assert_eq!(
            e("1.256.3.4"),
            "ParseAddr(\"1.256.3.4\"): IPv4 field has value >255"
        );
        assert_eq!(
            e("1..3.4"),
            "ParseAddr(\"1..3.4\"): IPv4 field must have at least one digit (at \".3.4\")"
        );
        assert_eq!(
            e("1.2.3.x"),
            "ParseAddr(\"1.2.3.x\"): unexpected character (at \"x\")"
        );
        assert_eq!(e("%x"), "ParseAddr(\"%x\"): missing IPv6 address");
        assert_eq!(
            e("12345::"),
            "ParseAddr(\"12345::\"): each group must have 4 or less digits (at \"12345::\")"
        );
        assert_eq!(
            e("1:2:3:4:5:6:7:8:9"),
            "ParseAddr(\"1:2:3:4:5:6:7:8:9\"): trailing garbage after address (at \"9\")"
        );
        assert_eq!(e("1:2"), "ParseAddr(\"1:2\"): address string too short");
        assert_eq!(
            e("1:2:3:4:5:6:7::8"),
            "ParseAddr(\"1:2:3:4:5:6:7::8\"): the :: must expand to at least one field of zeros"
        );
        assert_eq!(
            e("1:2:3:4:5:1.2.3.4"),
            "ParseAddr(\"1:2:3:4:5:1.2.3.4\"): embedded IPv4 address must replace the final 2 fields of the address (at \"1.2.3.4\")"
        );
        assert_eq!(
            e("1:x"),
            "ParseAddr(\"1:x\"): each colon-separated field must have at least one digit (at \"x\")"
        );
        assert_eq!(
            e("1:"),
            "ParseAddr(\"1:\"): colon must be followed by more characters (at \":\")"
        );
        assert_eq!(e("1x"), "ParseAddr(\"1x\"): unable to parse IP");
        assert_eq!(
            e("1:2x"),
            "ParseAddr(\"1:2x\"): unexpected character, want colon (at \"x\")"
        );
        assert!(parse_addr(b"::").is_ok());
        assert!(parse_addr(b"::ffff:1.2.3.4").is_ok());
        assert!(parse_addr(b"fe80::1%eth0").is_ok());
        assert!(matches!(parse_addr(b"1.2.3.4"), Ok(Addr::V4)));
    }
}
