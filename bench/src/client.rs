//! One HTTP/1.1 keep-alive connection per worker.

use bytes::Bytes;
use http::header::{CONNECTION, CONTENT_TYPE, HOST};
use http::{Method, Request};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::{self, SendRequest};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

pub struct Conn {
    addr: String,
    host: String,
    tx: Option<SendRequest<Full<Bytes>>>,
}

pub struct Response {
    pub status: u16,
    pub body: Bytes,
}

impl Conn {
    pub fn new(addr: &str, host: &str) -> Conn {
        Conn {
            addr: addr.to_string(),
            host: host.to_string(),
            tx: None,
        }
    }

    async fn connect(&mut self) -> Result<(), String> {
        let stream = TcpStream::connect(&self.addr)
            .await
            .map_err(|e| format!("connect {}: {e}", self.addr))?;
        let _ = stream.set_nodelay(true);
        let (tx, conn) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        self.tx = Some(tx);
        Ok(())
    }

    /// Sends one request. A connection found closed before sending is reopened once; a
    /// failure after sending is returned (the request is not replayed).
    pub async fn send(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Bytes>,
    ) -> Result<Response, String> {
        let mut reopened = false;
        loop {
            if self.tx.is_none() {
                self.connect().await?;
            }
            let tx = self.tx.as_mut().expect("connected");
            if tx.ready().await.is_err() {
                self.tx = None;
                if reopened {
                    return Err("connection closed".into());
                }
                reopened = true;
                continue;
            }
            let mut b = Request::builder()
                .method(method.clone())
                .uri(path)
                .header(HOST, &self.host);
            if body.is_some() {
                b = b.header(CONTENT_TYPE, "application/json");
            }
            let req = b
                .body(Full::new(body.clone().unwrap_or_default()))
                .map_err(|e| e.to_string())?;
            let resp = match tx.send_request(req).await {
                Ok(r) => r,
                Err(e) => {
                    self.tx = None;
                    return Err(e.to_string());
                }
            };
            let status = resp.status().as_u16();
            let close = resp
                .headers()
                .get(CONNECTION)
                .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"close"));
            let body = match resp.into_body().collect().await {
                Ok(b) => b.to_bytes(),
                Err(e) => {
                    self.tx = None;
                    return Err(e.to_string());
                }
            };
            if close {
                self.tx = None;
            }
            return Ok(Response { status, body });
        }
    }
}
