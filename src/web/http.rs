//! Request and response types of the web layer.

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use http_body_util::Full;
use serde::Serialize;
use std::net::SocketAddr;

pub type Resp = http::Response<Full<Bytes>>;

pub const JSON: &str = "application/json";
pub const TEXT: &str = "text/plain; charset=utf-8";

/// Name recorded by the HTTP metrics middleware (Go: `c.Locals("metricName", ...)`).
#[derive(Clone, Copy, Debug)]
pub struct MetricName(pub &'static str);

/// A buffered request, as Fiber sees it.
pub struct Req {
    pub method: Method,
    /// Path as received, still percent-encoded (Fiber `UnescapePath: false`).
    pub path: String,
    /// Decoded query parameters, in order.
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// Raw path parameters of the matched route.
    pub params: Vec<(&'static str, String)>,
    pub remote: SocketAddr,
}

impl Req {
    /// First value of a query parameter, `""` when absent (Fiber `c.Query`).
    pub fn query(&self, name: &str) -> &str {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map_or("", |(_, v)| v.as_str())
    }

    /// Raw value of a path parameter.
    pub fn param(&self, name: &str) -> &str {
        self.params
            .iter()
            .find(|(k, _)| *k == name)
            .map_or("", |(_, v)| v.as_str())
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    /// Last value of a header: fasthttp keeps the last `Content-Type` of a request.
    pub fn last_header(&self, name: &str) -> &str {
        self.headers
            .get_all(name)
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

pub fn parse_query(raw: Option<&str>) -> Vec<(String, String)> {
    match raw {
        Some(q) => form_urlencoded::parse(q.as_bytes()).into_owned().collect(),
        None => Vec::new(),
    }
}

pub fn response(status: u16, content_type: &str, body: impl Into<Bytes>) -> Resp {
    let mut r = http::Response::new(Full::new(body.into()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(v) = HeaderValue::from_str(content_type) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    r
}

/// Fiber `c.JSON(v)` with an explicit status.
pub fn json<T: Serialize + ?Sized>(status: u16, v: &T) -> Resp {
    response(status, JSON, crate::gojson::to_vec(v))
}

/// Fiber `c.SendString(s)`.
pub fn text(status: u16, s: impl Into<String>) -> Resp {
    response(status, TEXT, s.into())
}

/// Fiber `c.SendStatus(status)` with an empty body: the status text.
pub fn status_text(status: u16) -> Resp {
    let reason = StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("");
    text(status, reason)
}

/// Escapes like Go's `html.EscapeString`.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

pub fn with_metric(mut r: Resp, name: &'static str) -> Resp {
    r.extensions_mut().insert(MetricName(name));
    r
}
