//! A router with Fiber v2's matching rules (strict routing, case sensitive, raw paths):
//! routes are tried in registration order and the first match wins; a path that matches
//! only for other methods answers 405 with an `Allow` header; GET routes also answer HEAD.

use super::App;
use super::http::{Req, Resp, html_escape, text};
use http::{HeaderValue, Method};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type HandlerFuture = Pin<Box<dyn Future<Output = Resp> + Send>>;
pub type Handler = fn(Arc<App>, Req) -> HandlerFuture;

#[derive(Clone, Debug, PartialEq)]
enum Seg {
    Static(&'static str),
    Param(&'static str),
    Wildcard,
}

struct Route {
    method: Method,
    segs: Vec<Seg>,
    handler: Handler,
    /// Registered by a Fiber middleware (`app.Use`): not counted for 405 answers.
    middleware: bool,
}

#[derive(Default)]
pub struct Router {
    routes: Vec<Route>,
}

/// Methods in the order Fiber lists them (`Config.RequestMethods`).
const METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

/// Fiber answers other methods before any middleware runs.
pub fn is_known_method(m: &Method) -> bool {
    METHODS.contains(&m.as_str())
}

fn parse_pattern(pattern: &'static str) -> Vec<Seg> {
    pattern[1..]
        .split('/')
        .map(|s| {
            if s == "*" {
                Seg::Wildcard
            } else if let Some(name) = s.strip_prefix(':') {
                Seg::Param(name)
            } else {
                Seg::Static(s)
            }
        })
        .collect()
}

/// Matches a raw path; returns the captured parameters (`*` for the wildcard).
fn match_path(segs: &[Seg], path: &str) -> Option<Vec<(&'static str, String)>> {
    let rest = path.strip_prefix('/')?;
    let parts: Vec<&str> = rest.split('/').collect();
    let mut params = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        match seg {
            Seg::Wildcard => {
                // "/docs/*" also matches "/docs".
                let tail = if i < parts.len() {
                    parts[i..].join("/")
                } else {
                    String::new()
                };
                params.push(("*", tail));
                return Some(params);
            }
            Seg::Static(s) => {
                if parts.get(i) != Some(s) {
                    return None;
                }
            }
            Seg::Param(name) => match parts.get(i) {
                Some(p) if !p.is_empty() => params.push((name, (*p).to_string())),
                _ => return None,
            },
        }
    }
    (parts.len() == segs.len()).then_some(params)
}

impl Router {
    pub fn add(&mut self, method: Method, pattern: &'static str, handler: Handler) {
        let segs = parse_pattern(pattern);
        if method == Method::GET {
            self.routes.push(Route {
                method: Method::HEAD,
                segs: segs.clone(),
                handler,
                middleware: false,
            });
        }
        self.routes.push(Route {
            method,
            segs,
            handler,
            middleware: false,
        });
    }

    /// A GET-only handler of a Fiber middleware (healthcheck): no HEAD, no 405 for other
    /// methods.
    pub fn middleware_get(&mut self, pattern: &'static str, handler: Handler) {
        self.routes.push(Route {
            method: Method::GET,
            segs: parse_pattern(pattern),
            handler,
            middleware: true,
        });
    }

    pub fn get(&mut self, pattern: &'static str, h: Handler) {
        self.add(Method::GET, pattern, h);
    }

    pub fn post(&mut self, pattern: &'static str, h: Handler) {
        self.add(Method::POST, pattern, h);
    }

    pub fn delete(&mut self, pattern: &'static str, h: Handler) {
        self.add(Method::DELETE, pattern, h);
    }

    /// Finds the handler of a request, or the Fiber error response.
    pub fn route(&self, req: &mut Req) -> Result<Handler, Box<Resp>> {
        for r in &self.routes {
            if r.method == req.method
                && let Some(params) = match_path(&r.segs, &req.path)
            {
                req.params = params;
                return Ok(r.handler);
            }
        }
        Err(Box::new(self.not_found(&req.method, &req.path)))
    }

    /// 404, or 405 with `Allow` when other methods match the path.
    pub fn not_found(&self, method: &Method, path: &str) -> Resp {
        let allowed: Vec<&str> = METHODS
            .iter()
            .filter(|m| {
                self.routes.iter().any(|r| {
                    !r.middleware && r.method.as_str() == **m && match_path(&r.segs, path).is_some()
                })
            })
            .copied()
            .collect();
        if allowed.is_empty() {
            return text(404, format!("Cannot {method} {}", html_escape(path)));
        }
        let mut r = text(405, "Method Not Allowed");
        if let Ok(v) = HeaderValue::from_str(&allowed.join(", ")) {
            r.headers_mut().insert(http::header::ALLOW, v);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let p = parse_pattern("/api/v1/job/:jobuuid");
        assert_eq!(
            match_path(&p, "/api/v1/job/abc"),
            Some(vec![("jobuuid", "abc".into())])
        );
        assert_eq!(match_path(&p, "/api/v1/job/"), None);
        assert_eq!(match_path(&p, "/api/v1/job/abc/"), None);
        assert_eq!(match_path(&p, "/API/v1/job/abc"), None);
        let p = parse_pattern("/api/v1/job/");
        assert!(match_path(&p, "/api/v1/job/").is_some());
        assert!(match_path(&p, "/api/v1/job").is_none());
        let p = parse_pattern("/docs/*");
        assert_eq!(match_path(&p, "/docs"), Some(vec![("*", "".into())]));
        assert_eq!(match_path(&p, "/docs/"), Some(vec![("*", "".into())]));
        assert_eq!(
            match_path(&p, "/docs/a/b.js"),
            Some(vec![("*", "a/b.js".into())])
        );
        let p = parse_pattern("/");
        assert!(match_path(&p, "/").is_some());
        assert!(match_path(&p, "//").is_none());
        assert!(match_path(&parse_pattern("/ping"), "/ping").is_some());
    }
}
