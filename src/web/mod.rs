//! HTTP server (`web/`, `web/webserver/`): Fiber-compatible routing and middlewares on hyper.

mod access_log;
pub mod cors;
pub mod handlers;
pub mod http;
mod monitor;
pub mod router;
mod swagger;

use self::http::{MetricName, Req, Resp, parse_query, text};
use crate::config::Config;
use crate::log::{self, Logger};
use crate::metrics::prom::Prom;
use crate::service::Service;
use ::http::{HeaderValue, Method, header};
use futures_util::FutureExt;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use router::Router;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};

/// Fiber `BodyLimit`.
pub const BODY_LIMIT: usize = 10_485_760;
/// Fiber `Concurrency`.
pub const MAX_CONNECTIONS: usize = 262_144;
/// Fiber read/write/idle timeouts.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Requests from the admin API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Shutdown,
    Restart,
}

/// How the server stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Stopped,
    Restart,
}

pub struct App {
    pub svc: Arc<Service>,
    pub cfg: Arc<Config>,
    pub schema: Option<crate::schema::Schema>,
    pub logger: Logger,
    router: Router,
    cors: Option<cors::Cors>,
    prom: Option<Arc<Prom>>,
    control: mpsc::UnboundedSender<Control>,
    connections: AtomicI64,
    monitor: monitor::Monitor,
}

macro_rules! h {
    ($f:path) => {
        |app, req| Box::pin($f(app, req))
    };
}

fn routes(cfg: &Config) -> Router {
    use handlers::*;
    let mut r = Router::default();
    // Registration order of the Go server (first match wins).
    if cfg.webserver.metrics.enable {
        r.get("/metrics", h!(metrics));
    }
    r.get("/", h!(home));
    r.get("/api/v1/job/:jobuuid", h!(get_job));
    r.post("/api/v1/job/", h!(create_job));
    r.post("/api/v1/job/pull", h!(pull_job));
    r.post("/api/v1/job/:jobuuid/succeed", h!(succeed_job));
    r.post("/api/v1/job/:jobuuid/cancel", h!(cancel_job));
    r.post("/api/v1/job/:jobuuid/fail", h!(fail_job));
    r.post(
        "/api/v1/job/:jobuuid/visibilitytimeout",
        h!(visibility_timeout),
    );
    r.post("/api/v1/job/:jobuuid/clone", h!(clone_job));
    r.delete("/api/v1/job/:jobuuid", h!(delete_job));
    r.get("/api/v1/jobs/", h!(get_jobs));
    r.get("/api/v1/jobs/all", h!(get_all_jobs));
    r.get("/api/v1/jobs/oldest", h!(get_oldest_jobs));
    r.get("/api/v1/jobs/stalled", h!(get_stalled_jobs));
    r.get("/api/v1/jobs/recent", h!(get_recent_jobs));
    r.delete("/api/v1/jobs/queued", h!(delete_queued_jobs));
    r.delete("/api/v1/jobs/", h!(delete_all_jobs));
    r.get("/api/v1/topics/", h!(get_topics));
    r.get("/api/v1/resources/", h!(get_locked_resources));
    r.post("/api/v1/resources/unlock", h!(unlock_all_resources));
    r.post("/api/v1/resource/:resourcename/unlock", h!(unlock_resource));
    r.post("/api/v1/admin/server/shutdown", h!(admin_shutdown));
    r.post("/api/v1/admin/server/restart", h!(admin_restart));
    r.get("/api/v1/system/info", h!(system_info));
    r.get("/api/v1/system/health", h!(system_health));
    r.get("/api/v1/system/resources", h!(system_resources));
    if cfg.webserver.metrics.enable {
        r.get("/api/v1/metrics/jobs/processing", h!(jobs_metrics));
        r.get("/api/v1/metrics/jobs/recent", h!(get_recent_jobs));
        r.get("/api/v1/metrics/jobs/stalled", h!(get_stalled_jobs));
        r.get("/api/v1/metrics/jobs/activity", h!(jobs_activity));
        r.get("/api/v1/metrics/jobs/topics", h!(jobs_metrics_topics));
        r.get("/api/v1/metrics/resources", h!(resources_metrics));
        r.get("/api/v1/metrics/topics", h!(topics_stats));
    }
    r.get("/ping", h!(ping));
    r.get("/healthcheck", h!(healthcheck));
    // Fiber healthcheck middleware: GET only (HEAD and other methods fall through to 404).
    r.middleware_get("/livez", h!(livez));
    r.middleware_get("/readyz", h!(readyz));
    if cfg.webserver.monitor.enable {
        r.get("/monitor", h!(monitor));
    }
    if cfg.webserver.swagger.enable {
        r.get("/docs/*", h!(docs));
    }
    r
}

impl App {
    pub fn request(&self, c: Control) {
        let _ = self.control.send(c);
    }

    pub fn open_connections(&self) -> i64 {
        self.connections.load(Ordering::Relaxed)
    }

    /// Middlewares and routing for one buffered request.
    async fn dispatch(self: Arc<Self>, mut req: Req) -> Resp {
        let method = req.method.clone();
        if !router::is_known_method(&method) {
            return text(400, "Invalid http method");
        }
        let cors_headers = match &self.cors {
            Some(c) => match c.handle(&req.method, &req.headers) {
                Ok(h) => Some(h),
                Err(preflight) => return *preflight,
            },
            None => None,
        };
        // `GET /metrics` is registered before the Prometheus middleware: not counted.
        let before_middleware =
            req.path == "/metrics" && (method == Method::GET || method == Method::HEAD);
        let in_progress = self
            .prom
            .as_ref()
            .filter(|_| !before_middleware)
            .map(|p| p.http.in_progress.with_label_values(&[method.as_str()]));
        if let Some(g) = &in_progress {
            g.inc();
        }
        let start = Instant::now();
        let mut resp = match self.router.route(&mut req) {
            Ok(handler) => {
                let fut = handler(self.clone(), req);
                match AssertUnwindSafe(fut).catch_unwind().await {
                    Ok(r) => r,
                    Err(_) => {
                        self.logger.error(
                            "Panic while handling a request",
                            &[log::s("topic", "server")],
                        );
                        text(500, "Internal Server Error")
                    }
                }
            }
            Err(r) => *r,
        };
        if let Some(g) = &in_progress {
            g.dec();
        }
        if let (Some(p), Some(MetricName(name))) =
            (&self.prom, resp.extensions().get::<MetricName>())
        {
            let status = resp.status().as_u16().to_string();
            p.http
                .requests_total
                .with_label_values(&[&status, method.as_str(), name])
                .inc();
            p.http
                .request_duration
                .with_label_values(&[&status, method.as_str(), name])
                .observe(start.elapsed().as_secs_f64());
        }
        if let Some(h) = cors_headers {
            cors::apply(&mut resp, h);
        }
        resp
    }
}

pub(crate) fn too_large() -> Resp {
    let mut r = text(413, "Request Entity Too Large");
    r.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    // fasthttp's status line (hyper's default reason is "Payload Too Large").
    r.extensions_mut()
        .insert(hyper::ext::ReasonPhrase::from_static(
            b"Request Entity Too Large",
        ));
    r
}

/// Reads the whole body (fasthttp does it before Fiber runs), then dispatches.
async fn serve(
    app: Arc<App>,
    req: ::http::Request<Incoming>,
    remote: SocketAddr,
) -> Result<Resp, Infallible> {
    let start = Instant::now();
    let (parts, body) = req.into_parts();
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > BODY_LIMIT as u64) {
        return Ok(too_large());
    }
    let body = match tokio::time::timeout(TIMEOUT, Limited::new(body, BODY_LIMIT).collect()).await {
        Ok(Ok(b)) => b.to_bytes(),
        Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => return Ok(too_large()),
        _ => {
            let mut r = text(408, "Request Timeout");
            r.headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
            return Ok(r);
        }
    };
    let path = parts.uri.path().to_string();
    let is_head = parts.method == Method::HEAD;
    let req = Req {
        method: parts.method.clone(),
        path: path.clone(),
        query: parse_query(parts.uri.query()),
        headers: parts.headers,
        body,
        params: Vec::new(),
        remote,
    };
    let mut resp = app.clone().dispatch(req).await;
    if is_head {
        // hyper does not send a HEAD body; keep the length of the GET answer like fasthttp.
        let len = hyper::body::Body::size_hint(resp.body())
            .exact()
            .unwrap_or(0);
        resp.headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    if app.cfg.webserver.logs.enable && router::is_known_method(&parts.method) {
        access_log::write(
            resp.status().as_u16(),
            remote,
            start.elapsed(),
            parts.method.as_str(),
            &path,
        );
    }
    Ok(resp)
}

/// Splits a Go `host:port` address; IPv4 only, like Fiber's default `tcp4` network.
async fn resolve_listen_address(address: &str) -> Result<SocketAddr, String> {
    let err = |m: &str| format!("listen tcp4 {address}: {m}");
    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| err("missing port in address"))?;
    let port: u16 = if port.is_empty() {
        0
    } else {
        port.parse().map_err(|_| err("unknown port"))?
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Ok(SocketAddr::from(([0, 0, 0, 0], port)));
    }
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| err(&e.to_string()))?;
    addrs
        .into_iter()
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| err(&format!("address {host}: no suitable address found")))
}

/// Waits for SIGINT/SIGTERM/SIGQUIT (stop) or SIGHUP (restart); Ctrl+C / Ctrl+Break on Windows.
async fn wait_signal(logger: &Logger) -> Control {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        let mut quit = signal(SignalKind::quit()).expect("SIGQUIT handler");
        let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        let f = |m: &str| {
            [
                log::s("topic", "server"),
                log::s("method", "HandleSignals"),
                log::s("signal", m),
            ]
        };
        tokio::select! {
            _ = term.recv() => {
                logger.info("Received SIGTERM, shutting down server...", &f("SIGTERM")[..2]);
                Control::Shutdown
            }
            _ = int.recv() => {
                logger.info("Received SIGINT (Ctrl+C), shutting down server...", &f("SIGINT")[..2]);
                Control::Shutdown
            }
            _ = quit.recv() => {
                logger.info("Received SIGQUIT, shutting down server...", &f("SIGQUIT")[..2]);
                Control::Shutdown
            }
            _ = hup.recv() => {
                logger.info("Reloading configuration and restart server...", &f("SIGHUP")[..2]);
                Control::Restart
            }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let mut c = windows::ctrl_c().expect("Ctrl+C handler");
        let mut b = windows::ctrl_break().expect("Ctrl+Break handler");
        let mut close = windows::ctrl_close().expect("close handler");
        let mut shutdown = windows::ctrl_shutdown().expect("shutdown handler");
        tokio::select! {
            _ = c.recv() => {}
            _ = b.recv() => {}
            _ = close.recv() => {}
            _ = shutdown.recv() => {}
        }
        logger.info(
            "Received SIGINT (Ctrl+C), shutting down server...",
            &[log::s("topic", "server"), log::s("method", "HandleSignals")],
        );
        Control::Shutdown
    }
}

/// Accepts connections until `stop_accept` changes. Each connection holds a clone of
/// `done`, so the receiver side sees the channel close when all of them have ended; on
/// `conn_stop` they finish the request in progress and close.
async fn accept_loop(
    app: Arc<App>,
    listener: TcpListener,
    mut stop_accept: tokio::sync::watch::Receiver<bool>,
    conn_stop: tokio::sync::watch::Receiver<bool>,
    done: mpsc::Sender<()>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(TIMEOUT)
        .keep_alive(true)
        .title_case_headers(true)
        .max_buf_size(8192);
    loop {
        let (stream, remote) = tokio::select! {
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            },
            _ = stop_accept.changed() => return,
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        app.connections.fetch_add(1, Ordering::Relaxed);
        let for_service = app.clone();
        let svc = service_fn(move |req| serve(for_service.clone(), req, remote));
        let conn = builder.serve_connection(TokioIo::new(stream), svc);
        let mut stop = conn_stop.clone();
        let done = done.clone();
        let app = app.clone();
        tokio::spawn(async move {
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => {}
                _ = stop.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                }
            }
            app.connections.fetch_sub(1, Ordering::Relaxed);
            drop(permit);
            drop(done);
        });
    }
}

/// Runs the service and the HTTP server until a stop or restart request (`RunServer`).
pub async fn run(cfg: Arc<Config>, version: &str, logger: Logger) -> Result<Outcome, String> {
    logger.info(
        "Server version",
        &[log::s("topic", "server"), log::s("version", version)],
    );
    let svc = Service::create(cfg.clone(), version, logger.clone()).await?;
    let init_error = |e: String| {
        logger.error(
            "Cannot initialize server",
            &[log::s("topic", "server"), log::err(&e)],
        );
        e
    };
    let schema = if cfg.jobs.json_schema.enable {
        let path = &cfg.jobs.json_schema.path;
        let bytes = std::fs::read(path).map_err(|e| {
            init_error(format!(
                "error reading schema file: {}",
                crate::config::go_open_error(path, &e)
            ))
        })?;
        Some(
            crate::schema::Schema::compile(&bytes)
                .map_err(|e| init_error(format!("error unmarshaling schema: {e}")))?,
        )
    } else {
        None
    };
    let cors = if cfg.webserver.cors.enable {
        Some(
            cors::Cors::new(
                &cfg.webserver.cors.allow_origins,
                &cfg.webserver.cors.allow_headers,
            )
            .map_err(init_error)?,
        )
    } else {
        None
    };
    if cfg.webserver.swagger.enable && cfg!(not(feature = "swagger-ui")) {
        logger.warn(
            "Swagger UI not available (built without the swagger-ui feature), only /docs/doc.json is served",
            &[log::s("topic", "server")],
        );
    }
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let app = Arc::new(App {
        prom: svc.metrics().prom().cloned(),
        svc: svc.clone(),
        cfg: cfg.clone(),
        schema,
        logger: logger.clone(),
        router: routes(&cfg),
        cors,
        control: control_tx,
        connections: AtomicI64::new(0),
        monitor: monitor::Monitor::default(),
    });
    let consul = if cfg.consul.enable {
        let c = crate::consul::Consul::new(&cfg.consul, logger.clone()).map_err(init_error)?;
        if let Err(e) = c.register().await {
            svc.stop().await;
            return Err(e);
        }
        Some(c)
    } else {
        None
    };
    svc.start();

    let (stop_accept_tx, stop_accept_rx) = tokio::sync::watch::channel(false);
    let (conn_stop_tx, conn_stop_rx) = tokio::sync::watch::channel(false);
    let (conns_done_tx, mut conns_done_rx) = mpsc::channel::<()>(1);
    let (listen_err_tx, mut listen_err_rx) = mpsc::unbounded_channel::<String>();
    let mut accept_task = None;
    if cfg.webserver.http.enable {
        let address = cfg.webserver.http.address.clone();
        logger.info(
            "Start HTTP web server",
            &[
                log::s("topic", "server"),
                log::s("method", "Listen"),
                log::s("address", &address),
            ],
        );
        let listener = match resolve_listen_address(&address).await {
            Ok(a) => TcpListener::bind(a)
                .await
                .map_err(|e| format!("listen tcp4 {address}: {e}")),
            Err(e) => Err(e),
        };
        match listener {
            Err(e) => {
                let _ = listen_err_tx.send(e);
            }
            Ok(listener) => {
                accept_task = Some(tokio::spawn(accept_loop(
                    app.clone(),
                    listener,
                    stop_accept_rx,
                    conn_stop_rx,
                    conns_done_tx.clone(),
                )));
            }
        }
    }
    drop(conns_done_tx);

    let outcome: Result<Outcome, String> = tokio::select! {
        c = wait_signal(&logger) => Ok(if c == Control::Restart { Outcome::Restart } else { Outcome::Stopped }),
        Some(c) = control_rx.recv() => {
            let f = [log::s("topic", "server"), log::s("method", "HandleSignals")];
            match c {
                Control::Shutdown => {
                    logger.info("Received SIGTERM, shutting down server...", &f);
                    Ok(Outcome::Stopped)
                }
                Control::Restart => {
                    logger.info("Reloading configuration and restart server...", &f);
                    Ok(Outcome::Restart)
                }
            }
        }
        Some(e) = listen_err_rx.recv() => {
            logger.error(
                "Web server stopped with error",
                &[log::s("topic", "server"), log::s("method", "HandleSignals"), log::err(&e)],
            );
            Err(e)
        }
    };

    // shutdownListener: stop accepting, end long polls, wait for in-flight requests.
    logger.info(
        "Shutdown Server ...",
        &[
            log::s("topic", "server"),
            log::s("method", "shutdownListener"),
        ],
    );
    let _ = stop_accept_tx.send(true);
    if let Some(t) = accept_task {
        let _ = t.await;
    }
    svc.begin_shutdown();
    let _ = conn_stop_tx.send(true);
    if tokio::time::timeout(TIMEOUT, conns_done_rx.recv())
        .await
        .is_err()
    {
        logger.error(
            "Server Shutdown",
            &[
                log::s("topic", "server"),
                log::s("method", "shutdownListener"),
                log::err("timeout waiting for connections to close"),
            ],
        );
    }
    logger.info(
        "Web server stopped",
        &[
            log::s("topic", "server"),
            log::s("method", "shutdownListener"),
        ],
    );
    match &outcome {
        Ok(Outcome::Stopped) => logger.info("Server stopped", &[log::s("topic", "server")]),
        Ok(Outcome::Restart) => logger.info("Restarting server ...", &[log::s("topic", "server")]),
        Err(_) => logger.error("Server stopped with error", &[log::s("topic", "server")]),
    }
    if let Some(c) = consul {
        c.deregister().await;
    }
    svc.stop().await;
    outcome
}
