//! Consul service registration (`web/webserver/webserver.go`, `hashicorp/consul/api`).
//!
//! The agent is configured like the Go client: `CONSUL_HTTP_ADDR` (default
//! `127.0.0.1:8500`, may include the scheme), `CONSUL_HTTP_SSL`, `CONSUL_HTTP_SSL_VERIFY`,
//! `CONSUL_CACERT`, `CONSUL_HTTP_TOKEN`, `CONSUL_HTTP_AUTH`. `consul.server.address/port` are
//! the address and port of the registered service.

use crate::config::ConsulConfig;
use crate::log::{self, Logger};
use serde::Serialize;

#[derive(Serialize)]
struct Check<'a> {
    #[serde(rename = "HTTP", skip_serializing_if = "str::is_empty")]
    http: &'a str,
    #[serde(rename = "Interval", skip_serializing_if = "str::is_empty")]
    interval: &'a str,
    #[serde(rename = "Timeout", skip_serializing_if = "str::is_empty")]
    timeout: &'a str,
}

#[derive(Serialize)]
struct Registration<'a> {
    #[serde(rename = "ID", skip_serializing_if = "str::is_empty")]
    id: &'a str,
    #[serde(rename = "Name", skip_serializing_if = "str::is_empty")]
    name: &'a str,
    #[serde(rename = "Tags", skip_serializing_if = "Option::is_none")]
    tags: Option<&'a [String]>,
    #[serde(rename = "Port", skip_serializing_if = "is_zero")]
    port: i64,
    #[serde(rename = "Address", skip_serializing_if = "str::is_empty")]
    address: &'a str,
    #[serde(rename = "Check")]
    check: Check<'a>,
    #[serde(rename = "Namespace", skip_serializing_if = "str::is_empty")]
    namespace: &'a str,
    #[serde(rename = "Partition", skip_serializing_if = "str::is_empty")]
    partition: &'a str,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

pub struct Consul {
    client: reqwest::Client,
    base: String,
    env_token: String,
    auth: Option<(String, String)>,
    cfg: ConsulConfig,
    logger: Logger,
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

impl Consul {
    pub fn new(cfg: &ConsulConfig, logger: Logger) -> Result<Consul, String> {
        Self::with_env(cfg, logger, env)
    }

    /// `new` with the environment read through `env`.
    fn with_env(
        cfg: &ConsulConfig,
        logger: Logger,
        env: impl Fn(&str) -> String,
    ) -> Result<Consul, String> {
        crate::tls::install_provider();
        let mut addr = env("CONSUL_HTTP_ADDR");
        if addr.is_empty() {
            addr = "127.0.0.1:8500".into();
        }
        let ssl = matches!(
            env("CONSUL_HTTP_SSL").to_lowercase().as_str(),
            "1" | "t" | "true"
        );
        let base = if addr.starts_with("http://") || addr.starts_with("https://") {
            addr.trim_end_matches('/').to_string()
        } else if let Some(sock) = addr.strip_prefix("unix://") {
            return Err(format!("Consul unix sockets are not supported: {sock}"));
        } else {
            format!("{}://{addr}", if ssl { "https" } else { "http" })
        };
        let mut builder = reqwest::Client::builder();
        if matches!(
            env("CONSUL_HTTP_SSL_VERIFY").to_lowercase().as_str(),
            "0" | "f" | "false"
        ) {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let ca = env("CONSUL_CACERT");
        if !ca.is_empty() {
            let pem = std::fs::read(&ca).map_err(|e| crate::config::go_open_error(&ca, &e))?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| e.to_string())?;
            builder = builder.add_root_certificate(cert);
        }
        let auth = env("CONSUL_HTTP_AUTH");
        let auth = (!auth.is_empty()).then(|| match auth.split_once(':') {
            Some((u, p)) => (u.to_string(), p.to_string()),
            None => (auth.clone(), String::new()),
        });
        Ok(Consul {
            client: builder.build().map_err(|e| e.to_string())?,
            base,
            env_token: env("CONSUL_HTTP_TOKEN"),
            auth,
            cfg: cfg.clone(),
            logger,
        })
    }

    fn request(&self, path: &str, token: &str) -> reqwest::RequestBuilder {
        let mut req = self.client.put(format!("{}{path}", self.base));
        let token = if token.is_empty() {
            &self.env_token
        } else {
            token
        };
        if !token.is_empty() {
            req = req.header("X-Consul-Token", token);
        }
        if let Some((u, p)) = &self.auth {
            req = req.basic_auth(u, Some(p));
        }
        req
    }

    async fn send(req: reqwest::RequestBuilder) -> Result<(), String> {
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(format!(
            "Unexpected response code: {} ({})",
            status.as_u16(),
            body.trim()
        ))
    }

    /// `RegisterConsul`.
    pub async fn register(&self) -> Result<(), String> {
        let c = &self.cfg;
        let reg = Registration {
            id: &c.service.id,
            name: &c.service.name,
            tags: c.service.tags.as_deref(),
            port: c.server.port,
            address: &c.server.address,
            check: Check {
                http: &c.health_check.url,
                interval: &c.health_check.interval,
                timeout: &c.health_check.timeout,
            },
            namespace: &c.service.namespace,
            partition: &c.service.partition,
        };
        let mut path = "/v1/agent/service/register".to_string();
        if c.server.replace_existing_checks {
            path.push_str("?replace-existing-checks=true");
        }
        let req = self
            .request(&path, &c.server.token)
            .header("Content-Type", "application/json")
            .body(crate::gojson::to_vec(&reg));
        match Self::send(req).await {
            Ok(()) => {
                self.logger.info(
                    "Consul service registered",
                    &[
                        log::s("topic", "server"),
                        log::s("method", "RegisterConsul"),
                    ],
                );
                Ok(())
            }
            Err(e) => {
                self.logger.error(
                    "Failed to register Consul service",
                    &[
                        log::s("topic", "server"),
                        log::s("method", "RegisterConsul"),
                        log::err(&e),
                    ],
                );
                Err(e)
            }
        }
    }

    /// `DeRegisterConsul`, with the token used for the registration (without it, an agent with
    /// ACLs refuses the call and the stopped service stays registered).
    pub async fn deregister(&self) {
        let path = format!("/v1/agent/service/deregister/{}", self.cfg.service.id);
        match Self::send(self.request(&path, &self.cfg.server.token)).await {
            Ok(()) => self.logger.info(
                "Consul service deregistered",
                &[
                    log::s("topic", "server"),
                    log::s("method", "DeRegisterConsul"),
                ],
            ),
            Err(e) => self.logger.error(
                "Failed to deregister Consul service",
                &[
                    log::s("topic", "server"),
                    log::s("method", "DeRegisterConsul"),
                    log::err(e),
                ],
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A Consul agent that answers 200 and records `(request line, X-Consul-Token)`.
    async fn agent() -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<(String, String)>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tcp.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let line = head.lines().next().unwrap_or("").to_string();
                let token = head
                    .lines()
                    .find_map(|l| l.strip_prefix("x-consul-token: "))
                    .unwrap_or("")
                    .to_string();
                let _ = tx.send((line, token));
                let _ = tcp
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn configured_token_is_used_to_register_and_deregister() {
        let (addr, mut rx) = agent().await;
        let mut cfg = ConsulConfig::default();
        cfg.server.token = "secret".into();
        cfg.service.id = "minijob".into();
        let env = |name: &str| match name {
            "CONSUL_HTTP_ADDR" => addr.clone(),
            _ => String::new(),
        };
        let consul = Consul::with_env(&cfg, Logger::nop(), env).unwrap();
        consul.register().await.unwrap();
        consul.deregister().await;
        let (line, token) = rx.recv().await.unwrap();
        assert!(
            line.starts_with("PUT /v1/agent/service/register "),
            "{line}"
        );
        assert_eq!(token, "secret");
        let (line, token) = rx.recv().await.unwrap();
        assert!(
            line.starts_with("PUT /v1/agent/service/deregister/minijob "),
            "{line}"
        );
        assert_eq!(token, "secret");
    }
}
