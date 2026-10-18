//! Minijob: a job queue and task scheduling service with an HTTP API.
//!
//! Rust port of the Go server, compatible with its API, configuration files and data.

pub mod apierror;
pub mod callback;
pub mod cli;
pub mod config;
pub mod consul;
pub mod eventlog;
pub mod events;
pub mod gojson;
pub mod gomail;
pub mod goregex;
pub mod gourl;
pub mod log;
pub mod metrics;
pub mod model;
pub mod schema;
pub mod service;
pub mod storage;
pub mod tls;
pub mod web;

/// Program version: `MINIJOB_VERSION` at build time (Go: `-ldflags -X main.Version`), else
/// `v` + the crate version.
pub const VERSION: &str = match option_env!("MINIJOB_VERSION") {
    Some(v) => v,
    None => concat!("v", env!("CARGO_PKG_VERSION")),
};
