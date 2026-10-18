//! CORS, port of Fiber v2.52 `middleware/cors` with the server's configuration
//! (`AllowOrigins`, `AllowHeaders`; default methods; no credentials, max-age or exposed
//! headers).

use super::http::Resp;
use http::{HeaderMap, HeaderValue, Method, header};

const ALLOW_METHODS: &str = "GET,POST,HEAD,PUT,DELETE,PATCH";

struct Subdomain {
    prefix: String,
    suffix: String,
}

pub struct Cors {
    all: bool,
    origins: Vec<String>,
    subdomains: Vec<Subdomain>,
    allow_headers: String,
}

/// Go `strings.ToLower` (`unicode.ToLower` on each character).
fn go_lower(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            // The only character whose full lowercase mapping is not a single character.
            '\u{130}' => 'i',
            c => c.to_lowercase().next().unwrap_or(c),
        })
        .collect()
}

/// `normalizeOrigin`: Go's `url.Parse`, then `scheme://host` in lowercase (host decoded, port
/// kept, user info dropped); refuses no host, a `*` in the host, a path other than `/`, a
/// query or a fragment.
fn normalize_origin(origin: &str) -> Option<String> {
    let u = crate::gourl::parse(origin).ok()?;
    if u.host.is_empty()
        || u.host.contains(&b'*')
        || !(u.path.is_empty() || u.path == b"/")
        || !u.raw_query.is_empty()
        || !u.fragment.is_empty()
    {
        return None;
    }
    Some(format!(
        "{}://{}",
        u.scheme,
        go_lower(&String::from_utf8_lossy(&u.host))
    ))
}

impl Cors {
    /// Validates the configuration like Fiber does at startup (where Go panics).
    pub fn new(allow_origins: &str, allow_headers: &str) -> Result<Cors, String> {
        let allow_origins = if allow_origins.is_empty() {
            "*"
        } else {
            allow_origins
        };
        let mut cors = Cors {
            all: allow_origins == "*",
            origins: Vec::new(),
            subdomains: Vec::new(),
            allow_headers: allow_headers.replace(' ', ""),
        };
        if !cors.all {
            // Entries are trimmed before locating the wildcard (Fiber used the untrimmed
            // position, so "a, https://*.b.com" never matched subdomains of b.com).
            for origin in allow_origins.split(',').map(str::trim) {
                if let Some(i) = origin.find("://*.") {
                    let trimmed = format!("{}{}", &origin[..i + 3], &origin[i + 4..]);
                    let trimmed = trimmed.trim();
                    let normalized = normalize_origin(trimmed).ok_or_else(|| {
                        format!("[CORS] Invalid origin format in configuration: {trimmed}")
                    })?;
                    cors.subdomains.push(Subdomain {
                        prefix: normalized[..i + 3].to_string(),
                        suffix: normalized[i + 3..].to_string(),
                    });
                } else {
                    let trimmed = origin.trim();
                    let normalized = normalize_origin(trimmed).ok_or_else(|| {
                        format!("[CORS] Invalid origin format in configuration: {trimmed}")
                    })?;
                    cors.origins.push(normalized);
                }
            }
        }
        Ok(cors)
    }

    fn allowed_origin(&self, origin: &str) -> Option<String> {
        if self.all {
            return Some("*".to_string());
        }
        if self.origins.iter().any(|o| o == origin) {
            return Some(origin.to_string());
        }
        self.subdomains
            .iter()
            .any(|s| {
                origin.len() >= s.prefix.len() + s.suffix.len()
                    && origin.starts_with(&s.prefix)
                    && origin.ends_with(&s.suffix)
            })
            .then(|| origin.to_string())
    }

    /// Runs before the router: answers preflights, otherwise returns the headers to add to
    /// the response.
    pub fn handle(
        &self,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<Vec<(header::HeaderName, String)>, Box<Resp>> {
        // Fiber reads the header bytes as they are (`to_str` refuses non-ASCII values).
        let origin = headers
            .get(header::ORIGIN)
            .map(|v| go_lower(&String::from_utf8_lossy(v.as_bytes())))
            .unwrap_or_default();
        let request_headers = headers
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let mut out: Vec<(header::HeaderName, String)> = Vec::new();
        let vary = |out: &mut Vec<(header::HeaderName, String)>, v: &str| {
            out.push((header::VARY, v.to_string()));
        };
        if origin.is_empty() {
            if !self.all {
                vary(&mut out, "Origin");
            }
            return Ok(out);
        }
        let request_method = headers
            .get(header::ACCESS_CONTROL_REQUEST_METHOD)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if *method == Method::OPTIONS && !request_method {
            vary(&mut out, "Origin");
            return Ok(out);
        }
        let allow = self.allowed_origin(&origin);
        if *method != Method::OPTIONS {
            if !self.all {
                vary(&mut out, "Origin");
            }
            if let Some(a) = allow {
                out.push((header::ACCESS_CONTROL_ALLOW_ORIGIN, a));
            }
            // `setCORSHeaders` without configured headers: the request's are reflected.
            if !request_headers.is_empty() {
                out.push((
                    header::ACCESS_CONTROL_ALLOW_HEADERS,
                    request_headers.to_string(),
                ));
            }
            return Ok(out);
        }
        // Preflight.
        let mut r = super::http::response(204, "", bytes::Bytes::new());
        r.headers_mut().remove(header::CONTENT_TYPE);
        let h = r.headers_mut();
        h.insert(
            header::VARY,
            HeaderValue::from_static(
                "Access-Control-Request-Method, Access-Control-Request-Headers, Origin",
            ),
        );
        if let Some(a) = allow.and_then(|a| HeaderValue::from_str(&a).ok()) {
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, a);
        }
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static(ALLOW_METHODS),
        );
        let allow_headers = if self.allow_headers.is_empty() {
            request_headers.to_string()
        } else {
            self.allow_headers.clone()
        };
        if let Some(v) = (!allow_headers.is_empty())
            .then(|| HeaderValue::from_str(&allow_headers).ok())
            .flatten()
        {
            h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
        }
        Err(Box::new(r))
    }
}

/// Adds the CORS headers to a response, merging `Vary` like Fiber's `c.Vary`.
pub fn apply(resp: &mut Resp, headers: Vec<(header::HeaderName, String)>) {
    for (name, value) in headers {
        if name == header::VARY {
            let existing = resp
                .headers()
                .get(header::VARY)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let merged = match existing {
                Some(e) if e.split(", ").any(|x| x == value) => e,
                Some(e) => format!("{e}, {value}"),
                None => value,
            };
            if let Ok(v) = HeaderValue::from_str(&merged) {
                resp.headers_mut().insert(header::VARY, v);
            }
        } else if let Ok(v) = HeaderValue::from_str(&value) {
            resp.headers_mut().insert(name, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn config_validation() {
        assert!(Cors::new("*", "").is_ok());
        assert!(Cors::new("", "").unwrap().all);
        assert!(Cors::new("https://a.com, http://b.com:8080", "").is_ok());
        assert!(Cors::new("https://*.a.com", "").is_ok());
        assert_eq!(
            Cors::new("a.com", "").err().unwrap(),
            "[CORS] Invalid origin format in configuration: a.com"
        );
        assert!(Cors::new("https://a.com/path", "").is_err());
    }

    /// Origins are parsed with Go's `url.Parse` rules, as Fiber does.
    #[test]
    fn origins_normalized_like_fiber() {
        let n = |o: &str| normalize_origin(o);
        assert_eq!(
            n("HTTPS://A.Com:8080/").as_deref(),
            Some("https://a.com:8080")
        );
        assert_eq!(n("https://user@a.com").as_deref(), Some("https://a.com"));
        assert_eq!(n("https://a.com?").as_deref(), Some("https://a.com"));
        assert_eq!(n("https://a.com#").as_deref(), Some("https://a.com"));
        assert_eq!(n("https://%C3%A9.com").as_deref(), Some("https://é.com"));
        assert_eq!(n("https://[::1]:80").as_deref(), Some("https://[::1]:80"));
        assert_eq!(n("https://\u{130}.com").as_deref(), Some("https://i.com"));
        for bad in [
            // Hosts may only percent-encode non-ASCII bytes.
            "https://ex%41mple.com",
            "https://a.com:abc",
            "1http://a.com",
            "https://a.com/p",
            "https://a.com?q",
            "https://a.com#f",
            "https://",
            "mailto:a@b.c",
            "a.com",
            "https://a b.com",
        ] {
            assert_eq!(n(bad), None, "{bad}");
        }
    }

    /// A simple request reflects `Access-Control-Request-Headers` (Fiber's `setCORSHeaders`
    /// without configured headers); a non-ASCII `Origin` is still an origin.
    #[test]
    fn simple_request_headers_like_fiber() {
        let c = Cors::new("*", "x-configured").unwrap();
        let h = c
            .handle(
                &Method::POST,
                &hm(&[
                    ("origin", "http://x.com"),
                    ("access-control-request-headers", "x-a"),
                ]),
            )
            .unwrap();
        assert_eq!(
            h,
            vec![
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".to_string()),
                (header::ACCESS_CONTROL_ALLOW_HEADERS, "x-a".to_string())
            ]
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_bytes("http://é.com".as_bytes()).unwrap(),
        );
        let h = c.handle(&Method::GET, &headers).unwrap();
        assert_eq!(
            h,
            vec![(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".to_string())]
        );
    }

    #[test]
    fn requests() {
        let c = Cors::new("*", "").unwrap();
        assert!(
            c.handle(&Method::GET, &HeaderMap::new())
                .unwrap()
                .is_empty()
        );
        let h = c
            .handle(&Method::GET, &hm(&[("origin", "http://x.com")]))
            .unwrap();
        assert_eq!(
            h,
            vec![(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".to_string())]
        );
        let pre = c
            .handle(
                &Method::OPTIONS,
                &hm(&[
                    ("origin", "http://x.com"),
                    ("access-control-request-method", "POST"),
                    ("access-control-request-headers", "content-type"),
                ]),
            )
            .unwrap_err();
        assert_eq!(pre.status(), 204);
        assert_eq!(
            pre.headers()["access-control-allow-headers"],
            "content-type"
        );
        assert_eq!(pre.headers()["access-control-allow-methods"], ALLOW_METHODS);
        let c = Cors::new("https://*.a.com,http://b.com", "").unwrap();
        let h = c
            .handle(&Method::GET, &hm(&[("origin", "https://x.a.com")]))
            .unwrap();
        assert!(h.contains(&(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            "https://x.a.com".into()
        )));
        let h = c
            .handle(&Method::GET, &hm(&[("origin", "http://evil.com")]))
            .unwrap();
        assert_eq!(h, vec![(header::VARY, "Origin".to_string())]);
    }
}
