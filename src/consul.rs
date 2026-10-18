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

    /// `DeRegisterConsul`.
    pub async fn deregister(&self) {
        let path = format!("/v1/agent/service/deregister/{}", self.cfg.service.id);
        match Self::send(self.request(&path, "")).await {
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
