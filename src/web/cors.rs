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

/// `normalizeOrigin`: `scheme://host[:port]` lowercase, no path, query or fragment.
fn normalize_origin(origin: &str) -> Option<String> {
    let (scheme, rest) = origin.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return None;
    }
    let (host, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if host.is_empty() || host.contains('*') || host.contains('@') {
        return None;
    }
    if !(path.is_empty() || path == "/") {
        return None;
    }
    Some(format!(
        "{}://{}",
        scheme.to_lowercase(),
        host.to_lowercase()
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
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
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
            headers
                .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
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
