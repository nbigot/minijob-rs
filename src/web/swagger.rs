//! `/docs/*` (gofiber/swagger v1.1.1): Swagger UI page, `doc.json`, swagger-ui assets.
//!
//! `index.html` and `doc.json` are the bytes served by the Go server; the assets come from
//! `swaggo/files v2.0.2` (source maps and the ES bundles are not embedded).

use super::http::{Req, Resp, response, text};
#[cfg(feature = "swagger-ui")]
use http::{HeaderValue, header};

const DOC_JSON: &str = include_str!("../../docs/doc.json");

#[cfg(feature = "swagger-ui")]
const INDEX_HTML: &str = include_str!("../../assets/swagger-index.html");

#[cfg(feature = "swagger-ui")]
const ASSETS: &[(&str, &str, &[u8])] = &[
    (
        "favicon-16x16.png",
        "image/png",
        include_bytes!("../../assets/swagger-ui/favicon-16x16.png"),
    ),
    (
        "favicon-32x32.png",
        "image/png",
        include_bytes!("../../assets/swagger-ui/favicon-32x32.png"),
    ),
    (
        "index.css",
        "text/css",
        include_bytes!("../../assets/swagger-ui/index.css"),
    ),
    (
        "index.html",
        "text/html",
        include_bytes!("../../assets/swagger-ui/index.html"),
    ),
    (
        "oauth2-redirect.html",
        "text/html",
        include_bytes!("../../assets/swagger-ui/oauth2-redirect.html"),
    ),
    (
        "swagger-initializer.js",
        "text/javascript",
        include_bytes!("../../assets/swagger-ui/swagger-initializer.js"),
    ),
    (
        "swagger-ui-bundle.js",
        "text/javascript",
        include_bytes!("../../assets/swagger-ui/swagger-ui-bundle.js"),
    ),
    (
        "swagger-ui-standalone-preset.js",
        "text/javascript",
        include_bytes!("../../assets/swagger-ui/swagger-ui-standalone-preset.js"),
    ),
    (
        "swagger-ui.css",
        "text/css",
        include_bytes!("../../assets/swagger-ui/swagger-ui.css"),
    ),
    (
        "swagger-ui.js",
        "text/javascript",
        include_bytes!("../../assets/swagger-ui/swagger-ui.js"),
    ),
];

/// `X-Forwarded-Prefix` handling of gofiber/swagger (computed per request; Go computed it
/// once, from the first request).
#[cfg(feature = "swagger-ui")]
fn prefix(req: &Req) -> String {
    let mut p = String::new();
    for v in req.headers.get_all("x-forwarded-prefix") {
        if let Ok(s) = v.to_str() {
            let trimmed = s.trim_end_matches('/');
            p.push_str(if trimmed.is_empty() { s } else { trimmed });
        }
    }
    p + "/docs/"
}

/// Go `path.Join(prefix, name)` for a clean absolute prefix.
#[cfg(feature = "swagger-ui")]
fn join(prefix: &str, name: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in prefix.split('/').chain(std::iter::once(name)) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

pub fn handle(req: &Req) -> Resp {
    let p = req.param("*");
    match p {
        #[cfg(feature = "swagger-ui")]
        "index.html" => {
            let url = join(&prefix(req), "doc.json");
            let mut quoted = String::new();
            crate::gojson::write_string(&mut quoted, &url);
            let html = INDEX_HTML.replacen(
                "\"url\":\"/docs/doc.json\"",
                &format!("\"url\":{quoted}"),
                1,
            );
            response(200, "text/html", html)
        }
        "doc.json" => response(200, "application/json", DOC_JSON),
        #[cfg(feature = "swagger-ui")]
        "" | "/" => {
            let mut r = response(301, "", bytes::Bytes::new());
            r.headers_mut().remove(header::CONTENT_TYPE);
            if let Ok(v) = HeaderValue::from_str(&join(&prefix(req), "index.html")) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
        _ => {
            #[cfg(feature = "swagger-ui")]
            if let Some((_, ctype, data)) = ASSETS.iter().find(|(n, _, _)| *n == p) {
                return response(200, ctype, bytes::Bytes::from_static(data));
            }
            text(
                404,
                format!("Cannot GET {}", super::http::html_escape(&req.path)),
            )
        }
    }
}

#[cfg(all(test, feature = "swagger-ui"))]
mod tests {
    use super::*;

    #[cfg(feature = "swagger-ui")]
    #[test]
    fn path_join() {
        assert_eq!(join("/docs/", "index.html"), "/docs/index.html");
        assert_eq!(join("/x/docs/", "doc.json"), "/x/docs/doc.json");
    }

    #[cfg(feature = "swagger-ui")]
    #[test]
    fn index_references_doc_json() {
        assert!(INDEX_HTML.contains("\"url\":\"/docs/doc.json\""));
    }
}
