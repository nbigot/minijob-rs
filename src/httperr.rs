//! Failures of outgoing HTTP requests (callbacks, Consul), worded like the `*url.Error` of
//! Go's `http.Client`: `Post "<url>": <cause>`.

use crate::gojson::quote;

/// `<Op> "<url>": <cause>`, the password of the URL hidden as `***`.
pub fn url_error(op: &str, url: &str, cause: &str) -> String {
    format!("{op} {}: {cause}", quote(&strip_password(url)))
}

/// Go's `stripPassword`: `user:***@`.
fn strip_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return url.to_string();
    };
    match rest[..at].split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***{}", &rest[at..]),
        None => url.to_string(),
    }
}

/// Why a request failed: the messages of the error's sources (reqwest's own message only
/// repeats the URL); a timeout reads like Go's.
pub fn cause(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "context deadline exceeded".to_string();
    }
    let mut parts = Vec::new();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        let m = s.to_string();
        // hyper-util's wrapper (`client error (Connect)`) adds nothing.
        if !m.starts_with("client error (") {
            parts.push(m);
        }
        source = s.source();
    }
    if parts.is_empty() {
        e.to_string()
    } else {
        parts.join(": ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_hidden() {
        assert_eq!(
            url_error("Put", "https://user:p@ss@example.com/a@b", "x"),
            "Put \"https://user:***@example.com/a@b\": x"
        );
        assert_eq!(strip_password("http://user@h/"), "http://user@h/");
        assert_eq!(strip_password("http://h/a:b@c"), "http://h/a:b@c");
        assert_eq!(strip_password("h:1"), "h:1");
    }

    #[tokio::test]
    async fn causes() {
        crate::tls::install_provider();
        let client = reqwest::Client::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let e = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        assert!(
            cause(&e).starts_with("tcp connect error: "),
            "{}",
            cause(&e)
        );
        // A server that never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let e = client
            .get(format!("http://{addr}/"))
            .timeout(std::time::Duration::from_millis(50))
            .send()
            .await
            .unwrap_err();
        assert_eq!(cause(&e), "context deadline exceeded");
        drop(listener);
    }
}
